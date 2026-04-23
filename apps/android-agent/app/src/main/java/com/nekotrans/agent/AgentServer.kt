package com.nekotrans.agent

import android.content.Context
import android.util.Base64
import io.github.rctcwyvrn.blake3.Blake3
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.InputStream
import java.io.OutputStream
import java.io.OutputStreamWriter
import java.io.RandomAccessFile
import java.net.InetSocketAddress
import java.net.ServerSocket
import java.net.SocketException
import java.net.SocketTimeoutException
import java.nio.charset.StandardCharsets
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.concurrent.thread

private const val MAX_IN_MEMORY_LOGS: Int = 512
private const val MAX_LOG_FILE_BYTES: Long = 1_048_576
private const val MAX_LOG_ARCHIVES: Int = 3
private const val VERIFY_BUFFER_BYTES: Int = 1024 * 1024

object AgentServer {
    const val PORT: Int = 38997

    private const val READ_TIMEOUT_MS: Int = 30000
    private const val PONG_JSON: String = "{\"type\":\"Pong\",\"protocol_version\":\"0.1\"}"
    private const val ERROR_JSON: String = "{\"type\":\"Error\",\"message\":\"unsupported command\"}"

    private val running = AtomicBoolean(false)
    private val taskLock = Any()
    private var activeTask: TaskSnapshot? = null
    private var currentFile: FileState? = null
    private var activeTargetRoot: File? = null
    private val logs: ArrayDeque<String> = ArrayDeque()
    private var serverSocket: ServerSocket? = null
    private var worker: Thread? = null

    fun start(context: Context? = null) {
        context?.applicationContext?.let { appContext ->
            AgentServerStorage.root = File(appContext.getExternalFilesDir(null), "wifi-skeleton")
        }

        if (!running.compareAndSet(false, true)) {
            return
        }

        worker = thread(name = "nekotrans-agent-capability", isDaemon = true) {
            try {
                ServerSocket().use { socket ->
                    socket.reuseAddress = true
                    socket.bind(InetSocketAddress(PORT))
                    serverSocket = socket

                    while (running.get()) {
                        try {
                            socket.accept().use { client ->
                                client.soTimeout = READ_TIMEOUT_MS
                                val input = client.getInputStream()
                                val request = try {
                                    readCommandLine(input)
                                } catch (_: SocketTimeoutException) {
                                    null
                                }
                                val rawOutput = client.getOutputStream()
                                if (!writeBinaryResponse(request, rawOutput)) {
                                    OutputStreamWriter(
                                        rawOutput,
                                        StandardCharsets.UTF_8,
                                    ).use { output ->
                                        output.write(responseFor(request, input))
                                        output.write("\n")
                                        output.flush()
                                    }
                                }
                            }
                        } catch (err: SocketException) {
                            if (running.get()) {
                                throw err
                            }
                        }
                    }
                }
            } finally {
                serverSocket = null
                running.set(false)
            }
        }
    }

    fun stop() {
        running.set(false)
        serverSocket?.close()
        serverSocket = null
        worker = null
    }

    fun isRunning(): Boolean = running.get()

    fun statusText(): String {
        return if (isRunning()) {
            "监听端口 $PORT"
        } else {
            "已停止"
        }
    }

    fun taskSummary(): AgentTaskSummary {
        return synchronized(taskLock) {
            val task = activeTask
            if (task == null) {
                AgentTaskSummary(
                    taskId = "None",
                    state = "Idle",
                    message = "等待桌面端任务",
                    files = "0 / 0",
                    chunks = "0",
                    bytes = "0 B",
                    lastPath = "-",
                )
            } else {
                AgentTaskSummary(
                    taskId = task.taskId,
                    state = task.state,
                    message = task.message,
                    files = "${task.filesCompleted} / ${task.filesStarted}",
                    chunks = task.ackedChunks.toString(),
                    bytes = formatBytes(task.bytesTransferred),
                    lastPath = task.lastRelativePath ?: "-",
                )
            }
        }
    }

    fun activeRoot(): File? {
        return synchronized(taskLock) {
            activeTargetRoot ?: AgentServerStorage.root
        }
    }

    private fun responseFor(request: String?, input: InputStream): String {
        val command = request?.trim()
        return when (command) {
            null, "", "HELLO" -> AgentCapability.current().toJson()
            "PING" -> PONG_JSON
            "TASK_SNAPSHOT" -> currentTaskSnapshotJson()
            "PAUSE_TASK" -> updateActiveTaskState("Paused")
            "RESUME_TASK" -> updateActiveTaskState("Running")
            "CANCEL_TASK" -> updateActiveTaskState("Cancelled")
            "FILE_SNAPSHOT" -> currentFileSnapshotJson()
            "LOG_SNAPSHOT" -> logSnapshot()
            else -> {
                if (command.startsWith("START_TASK ")) {
                    startTask(command.removePrefix("START_TASK ").trim())
                } else if (command.startsWith("SET_TARGET_ROOT ")) {
                    setTargetRoot(command.removePrefix("SET_TARGET_ROOT ").trim())
                } else if (command.startsWith("START_FILE ")) {
                    startFile(command.removePrefix("START_FILE ").trim())
                } else if (command.startsWith("COMPLETE_FILE ")) {
                    completeFile(command.removePrefix("COMPLETE_FILE ").trim())
                } else if (command.startsWith("CHUNK_ACK ")) {
                    acknowledgeChunk(command.removePrefix("CHUNK_ACK ").trim())
                } else if (command.startsWith("CHUNK_STATUS ")) {
                    chunkStatus(command.removePrefix("CHUNK_STATUS ").trim())
                } else if (command.startsWith("PUSH_CHUNK ")) {
                    pushChunk(command.removePrefix("PUSH_CHUNK ").trim())
                } else if (command.startsWith("PUSH_CHUNK_BIN ")) {
                    pushChunkBinary(command.removePrefix("PUSH_CHUNK_BIN ").trim(), input)
                } else if (command.startsWith("PUSH_CHUNK_BATCH_BIN ")) {
                    pushChunkBatchBinary(command.removePrefix("PUSH_CHUNK_BATCH_BIN ").trim(), input)
                } else if (command.startsWith("PUSH_FILE_BUNDLE_BIN ")) {
                    pushFileBundleBinary(command.removePrefix("PUSH_FILE_BUNDLE_BIN ").trim(), input)
                } else if (command.startsWith("PULL_CHUNK ")) {
                    pullChunk(command.removePrefix("PULL_CHUNK ").trim())
                } else if (command.startsWith("PULL_CHUNK_BIN ")) {
                    ERROR_JSON
                } else if (command.startsWith("STAT_FILE ")) {
                    statFile(command.removePrefix("STAT_FILE ").trim())
                } else if (command.startsWith("VERIFY_FILE ")) {
                    verifyFile(command.removePrefix("VERIFY_FILE ").trim())
                } else {
                    ERROR_JSON
                }
            }
        }
    }

    private fun writeBinaryResponse(request: String?, output: OutputStream): Boolean {
        val command = request?.trim() ?: return false
        if (!command.startsWith("PULL_CHUNK_BIN ")) {
            return false
        }
        pullChunkBinary(command.removePrefix("PULL_CHUNK_BIN ").trim(), output)
        return true
    }

    private fun startTask(taskId: String): String {
        if (taskId.isEmpty()) {
            return ERROR_JSON
        }

        return synchronized(taskLock) {
            val snapshot = TaskSnapshot(
                taskId = taskId,
                state = "Running",
                message = "task accepted",
                updatedAtEpochMs = nowEpochMs(),
            )
            activeTask = snapshot
            currentFile = null
            activeTargetRoot = AgentServerStorage.root
            appendLog("audit", taskId, "task accepted")
            snapshot.toJson()
        }
    }

    private fun setTargetRoot(targetRoot: String): String {
        val decodedTargetRoot = decodePathArg(targetRoot.trim()) ?: return ERROR_JSON
        if (decodedTargetRoot.isEmpty()) {
            return ERROR_JSON
        }

        return synchronized(taskLock) {
            activeTask ?: return@synchronized ERROR_JSON
            val root = resolveTargetRoot(decodedTargetRoot) ?: return@synchronized ERROR_JSON
            root.mkdirs()
            activeTargetRoot = root
            appendLog("audit", activeTask?.taskId ?: "", "target root set: ${root.path}")
            "{\"type\":\"Ok\",\"message\":\"target root accepted\",\"target_root\":\"${escapeJson(root.path)}\"}"
        }
    }

    private fun startFile(args: String): String {
        val separator = args.lastIndexOf(' ')
        if (separator <= 0 || separator == args.lastIndex) {
            return ERROR_JSON
        }

        val relativePath = decodePathArg(args.substring(0, separator).trim()) ?: return ERROR_JSON
        val sizeBytes = args.substring(separator + 1).trim().toLongOrNull()
        if (relativePath.isEmpty() || sizeBytes == null || sizeBytes < 0) {
            return ERROR_JSON
        }

        return synchronized(taskLock) {
            val task = activeTask ?: return@synchronized ERROR_JSON
            val updatedTask = task.copy(
                filesStarted = task.filesStarted + 1,
                lastRelativePath = relativePath,
                updatedAtEpochMs = nowEpochMs(),
            )
            activeTask = updatedTask
            val file = FileState(
                taskId = task.taskId,
                relativePath = relativePath,
                sizeBytes = sizeBytes,
            )
            currentFile = file
            appendLog("transfer", task.taskId, "file accepted: $relativePath")
            file.toJson("file accepted")
        }
    }

    private fun acknowledgeChunk(chunkIndexText: String): String {
        val chunkIndex = chunkIndexText.toIntOrNull()
        if (chunkIndex == null || chunkIndex < 0) {
            return ERROR_JSON
        }

        return synchronized(taskLock) {
            val task = activeTask ?: return@synchronized ERROR_JSON
            val file = currentFile ?: return@synchronized ERROR_JSON
            val added = file.ackedChunks.add(chunkIndex)
            file.lastChunkIndex = chunkIndex
            file.lastOffset = null
            file.lastLength = 0
            file.updatedAtEpochMs = nowEpochMs()
            if (added) {
                activeTask = task.copy(
                    ackedChunks = task.ackedChunks + 1,
                    lastRelativePath = file.relativePath,
                    updatedAtEpochMs = nowEpochMs(),
                )
            }
            appendLog("transfer", file.taskId, "chunk acknowledged: $chunkIndex")
            chunkStatusJson(
                file = file,
                chunkIndex = chunkIndex,
                offset = null,
                length = 0,
                status = if (added) "acknowledged" else "already_acknowledged",
                message = "chunk acknowledged",
            )
        }
    }

    private fun pushChunk(args: String): String {
        val parts = args.split(" ", limit = 4)
        if (parts.size != 4) {
            return ERROR_JSON
        }

        val relativePath = decodePathArg(parts[0]) ?: return ERROR_JSON
        val chunkIndex = parts[1].toIntOrNull()
        val offset = parts[2].toLongOrNull()
        if (relativePath.isEmpty() || chunkIndex == null || chunkIndex < 0 || offset == null || offset < 0) {
            return ERROR_JSON
        }

        val payload = try {
            Base64.decode(parts[3], Base64.NO_WRAP)
        } catch (_: IllegalArgumentException) {
            return ERROR_JSON
        }

        return writeChunk(relativePath, chunkIndex, offset, payload, "chunk written")
    }

    private fun pushChunkBinary(args: String, input: InputStream): String {
        val parts = args.split(" ")
        if (parts.size != 4) {
            return ERROR_JSON
        }

        val relativePath = decodePathArg(parts[0]) ?: return ERROR_JSON
        val chunkIndex = parts[1].toIntOrNull()
        val offset = parts[2].toLongOrNull()
        val length = parts[3].toIntOrNull()
        if (
            relativePath.isEmpty() ||
            chunkIndex == null ||
            chunkIndex < 0 ||
            offset == null ||
            offset < 0 ||
            length == null ||
            length < 0
        ) {
            return ERROR_JSON
        }

        val payload = try {
            readExactBytes(input, length)
        } catch (_: Exception) {
            return ERROR_JSON
        }

        return writeChunk(relativePath, chunkIndex, offset, payload, "binary chunk written")
    }

    private fun pushChunkBatchBinary(args: String, input: InputStream): String {
        val parts = args.split(" ")
        if (parts.size != 2) {
            return ERROR_JSON
        }

        val relativePath = decodePathArg(parts[0]) ?: return ERROR_JSON
        val chunkCount = parts[1].toIntOrNull()
        if (relativePath.isEmpty() || chunkCount == null || chunkCount < 0) {
            return ERROR_JSON
        }

        return synchronized(taskLock) {
            val task = activeTask ?: return@synchronized ERROR_JSON
            val file = currentFile ?: return@synchronized ERROR_JSON
            if (file.relativePath != relativePath) {
                return@synchronized ERROR_JSON
            }

            val target = tempFileFor(file.relativePath) ?: return@synchronized ERROR_JSON
            target.parentFile?.mkdirs()
            var writtenBytes = 0L
            var newChunks = 0
            val now = nowEpochMs()
            RandomAccessFile(target, "rw").use { output ->
                val buffer = ByteArray(1024 * 1024)
                repeat(chunkCount) {
                    val chunkIndex = readNetworkInt(input)
                    val offset = readNetworkLong(input)
                    val length = readNetworkInt(input)
                    if (chunkIndex < 0 || offset < 0 || length < 0) {
                        return@synchronized ERROR_JSON
                    }
                    output.seek(offset)
                    if (!copyExactBytes(input, output, length, buffer)) {
                        return@synchronized ERROR_JSON
                    }
                    if (file.ackedChunks.add(chunkIndex)) {
                        newChunks += 1
                        file.ackedBytes += length.toLong()
                    }
                    file.lastChunkIndex = chunkIndex
                    file.lastOffset = offset
                    file.lastLength = length
                    writtenBytes += length.toLong()
                }
            }
            file.updatedAtEpochMs = now
            activeTask = task.copy(
                ackedChunks = task.ackedChunks + newChunks,
                bytesTransferred = task.bytesTransferred + writtenBytes,
                lastRelativePath = file.relativePath,
                updatedAtEpochMs = now,
            )
            appendLog("transfer", file.taskId, "binary chunk batch written: $chunkCount")
            "{\"type\":\"ChunkBatchAck\",\"task_id\":\"${escapeJson(file.taskId)}\",\"relative_path\":\"${escapeJson(file.relativePath)}\",\"chunks\":$chunkCount,\"bytes\":$writtenBytes,\"status\":\"batch_written\",\"message\":\"binary chunk batch written\"}"
        }
    }

    private fun pushFileBundleBinary(args: String, input: InputStream): String {
        val parts = args.split(" ")
        if (parts.size != 3) {
            return ERROR_JSON
        }

        val bundleId = parts[0].filter { it.isLetterOrDigit() || it == '-' || it == '_' }
        val manifestLength = parts[1].toIntOrNull()
        val payloadLength = parts[2].toLongOrNull()
        if (bundleId.isEmpty() || manifestLength == null || manifestLength < 0 || payloadLength == null || payloadLength < 0) {
            return ERROR_JSON
        }

        val manifestText = try {
            readExactBytes(input, manifestLength).toString(StandardCharsets.UTF_8)
        } catch (_: Exception) {
            return ERROR_JSON
        }
        val entries = parseBundleManifest(manifestText) ?: return ERROR_JSON

        return synchronized(taskLock) {
            val task = activeTask ?: return@synchronized ERROR_JSON
            var writtenBytes = 0L
            var filesCompleted = 0
            var directoriesCreated = 0
            for (entry in entries) {
                if (entry.isDirectory) {
                    val directory = finalFileFor(entry.relativePath) ?: return@synchronized ERROR_JSON
                    if (directory.exists() && !directory.isDirectory) {
                        directory.delete()
                    }
                    if (!directory.exists() && !directory.mkdirs()) {
                        return@synchronized "{\"type\":\"Error\",\"message\":\"bundle directory create failed\"}"
                    }
                    directoriesCreated += 1
                    continue
                }
                val final = finalFileFor(entry.relativePath) ?: return@synchronized ERROR_JSON
                val temp = tempFileFor(entry.relativePath) ?: return@synchronized ERROR_JSON
                temp.parentFile?.mkdirs()
                if (entry.sizeBytes == 0L) {
                    if (temp.exists()) {
                        temp.delete()
                    }
                    temp.createNewFile()
                } else {
                    RandomAccessFile(temp, "rw").use { output ->
                        output.setLength(0)
                        var remaining = entry.sizeBytes
                        val buffer = ByteArray(minOf(1024 * 1024L, remaining).toInt().coerceAtLeast(1))
                        while (remaining > 0) {
                            val wanted = minOf(buffer.size.toLong(), remaining).toInt()
                            val read = input.read(buffer, 0, wanted)
                            if (read < 0) {
                                return@synchronized ERROR_JSON
                            }
                            output.write(buffer, 0, read)
                            remaining -= read.toLong()
                            writtenBytes += read.toLong()
                        }
                    }
                }
                final.parentFile?.mkdirs()
                if (final.exists()) {
                    final.delete()
                }
                if (!temp.renameTo(final)) {
                    return@synchronized "{\"type\":\"Error\",\"message\":\"bundle file finalize failed\"}"
                }
                if (entry.modifiedAtEpochMs > 0) {
                    final.setLastModified(entry.modifiedAtEpochMs)
                }
                filesCompleted += 1
            }
            if (writtenBytes != payloadLength) {
                return@synchronized "{\"type\":\"Error\",\"message\":\"bundle payload length mismatch\"}"
            }
            activeTask = task.copy(
                filesStarted = task.filesStarted + filesCompleted,
                filesCompleted = task.filesCompleted + filesCompleted,
                bytesTransferred = task.bytesTransferred + writtenBytes,
                lastRelativePath = entries.lastOrNull()?.relativePath,
                updatedAtEpochMs = nowEpochMs(),
            )
            appendLog("transfer", task.taskId, "file bundle written: $bundleId files=$filesCompleted dirs=$directoriesCreated bytes=$writtenBytes")
            "{\"type\":\"FileBundleAck\",\"task_id\":\"${escapeJson(task.taskId)}\",\"bundle_id\":\"${escapeJson(bundleId)}\",\"files\":$filesCompleted,\"directories\":$directoriesCreated,\"bytes\":$writtenBytes,\"status\":\"bundle_written\",\"message\":\"file bundle written\"}"
        }
    }

    private fun writeChunk(
        relativePath: String,
        chunkIndex: Int,
        offset: Long,
        payload: ByteArray,
        message: String,
    ): String {
        return synchronized(taskLock) {
            val task = activeTask ?: return@synchronized ERROR_JSON
            val file = currentFile ?: return@synchronized ERROR_JSON
            if (file.relativePath != relativePath) {
                return@synchronized ERROR_JSON
            }

            val target = tempFileFor(file.relativePath) ?: return@synchronized ERROR_JSON
            target.parentFile?.mkdirs()
            RandomAccessFile(target, "rw").use { output ->
                output.seek(offset)
                output.write(payload)
            }
            val added = file.ackedChunks.add(chunkIndex)
            if (added) {
                file.ackedBytes += payload.size.toLong()
                activeTask = task.copy(
                    ackedChunks = task.ackedChunks + 1,
                    bytesTransferred = task.bytesTransferred + payload.size.toLong(),
                    lastRelativePath = file.relativePath,
                    updatedAtEpochMs = nowEpochMs(),
                )
            } else {
                activeTask = task.copy(
                    lastRelativePath = file.relativePath,
                    updatedAtEpochMs = nowEpochMs(),
                )
            }
            file.lastChunkIndex = chunkIndex
            file.lastOffset = offset
            file.lastLength = payload.size
            file.updatedAtEpochMs = nowEpochMs()
            appendLog("transfer", file.taskId, "$message: $chunkIndex")
            chunkStatusJson(
                file = file,
                chunkIndex = chunkIndex,
                offset = offset,
                length = payload.size,
                status = if (added) "written" else "already_committed",
                message = message,
            )
        }
    }

    private fun chunkStatus(args: String): String {
        val parts = args.split(" ")
        if (parts.size != 4) {
            return ERROR_JSON
        }

        val relativePath = decodePathArg(parts[0].trim()) ?: return ERROR_JSON
        val chunkIndex = parts[1].trim().toIntOrNull()
        val offset = parts[2].trim().toLongOrNull()
        val length = parts[3].trim().toLongOrNull()
        if (relativePath.isEmpty() || chunkIndex == null || chunkIndex < 0 || offset == null || offset < 0 || length == null || length < 0) {
            return ERROR_JSON
        }

        return synchronized(taskLock) {
            val task = activeTask
            val file = currentFile
            val target = readableFileFor(relativePath)
            val expectedEnd = offset + length
            val committedOnDisk = target?.exists() == true && target.length() >= expectedEnd
            val status =
                when {
                    file?.relativePath == relativePath && file.ackedChunks.contains(chunkIndex) -> "committed"
                    committedOnDisk -> "committed_on_disk"
                    file != null && file.relativePath != relativePath -> "path_mismatch"
                    else -> "not_committed"
                }
            if (task != null) {
                activeTask = task.copy(
                    lastRelativePath = relativePath,
                    updatedAtEpochMs = nowEpochMs(),
                )
            }
            chunkProbeJson(
                taskId = task?.taskId ?: file?.taskId.orEmpty(),
                relativePath = relativePath,
                chunkIndex = chunkIndex,
                offset = offset,
                length = length.toInt(),
                status = status,
                ackedChunks = file?.ackedChunks?.size ?: 0,
                ackedBytes = file?.ackedBytes ?: 0,
                message = "chunk status",
            )
        }
    }

    private fun completeFile(relativePath: String): String {
        val cleanPath = decodePathArg(relativePath.trim()) ?: return ERROR_JSON
        if (cleanPath.isEmpty()) {
            return ERROR_JSON
        }

        return synchronized(taskLock) {
            val task = activeTask ?: return@synchronized ERROR_JSON
            val temp = tempFileFor(cleanPath) ?: return@synchronized ERROR_JSON
            val final = finalFileFor(cleanPath) ?: return@synchronized ERROR_JSON
            val file = currentFile
            if (!temp.exists() && file != null && file.relativePath == cleanPath && file.sizeBytes == 0L) {
                final.parentFile?.mkdirs()
                if (final.exists()) {
                    final.delete()
                }
                if (!final.createNewFile()) {
                    return@synchronized "{\"type\":\"Error\",\"message\":\"empty file finalize failed\"}"
                }
                file.completed = true
                file.updatedAtEpochMs = nowEpochMs()
                activeTask = task.copy(
                    filesCompleted = task.filesCompleted + 1,
                    lastRelativePath = cleanPath,
                    updatedAtEpochMs = nowEpochMs(),
                )
                appendLog("transfer", task.taskId, "empty file completed: $cleanPath")
                return@synchronized "{\"type\":\"Ok\",\"message\":\"file completed\",\"relative_path\":\"${escapeJson(cleanPath)}\"}"
            }
            if (!temp.exists() && final.exists()) {
                if (file != null && file.relativePath == cleanPath && !file.completed) {
                    file.completed = true
                    file.updatedAtEpochMs = nowEpochMs()
                    activeTask = task.copy(
                        filesCompleted = task.filesCompleted + 1,
                        lastRelativePath = cleanPath,
                        updatedAtEpochMs = nowEpochMs(),
                    )
                }
                appendLog("transfer", task.taskId, "file already completed: $cleanPath")
                return@synchronized "{\"type\":\"Ok\",\"message\":\"file already completed\",\"relative_path\":\"${escapeJson(cleanPath)}\"}"
            }
            if (!temp.exists()) {
                return@synchronized "{\"type\":\"Error\",\"message\":\"temp file not found\"}"
            }
            final.parentFile?.mkdirs()
            if (final.exists()) {
                final.delete()
            }
            if (!temp.renameTo(final)) {
                return@synchronized "{\"type\":\"Error\",\"message\":\"file finalize failed\"}"
            }
            if (file != null && file.relativePath == cleanPath && !file.completed) {
                file.completed = true
                file.updatedAtEpochMs = nowEpochMs()
                activeTask = task.copy(
                    filesCompleted = task.filesCompleted + 1,
                    lastRelativePath = cleanPath,
                    updatedAtEpochMs = nowEpochMs(),
                )
            }
            appendLog("transfer", activeTask?.taskId ?: "", "file completed: $cleanPath")
            "{\"type\":\"Ok\",\"message\":\"file completed\",\"relative_path\":\"${escapeJson(cleanPath)}\"}"
        }
    }

    private fun pullChunk(args: String): String {
        val parts = args.split(" ", limit = 3)
        if (parts.size != 3) {
            return ERROR_JSON
        }

        val relativePath = decodePathArg(parts[0]) ?: return ERROR_JSON
        val offset = parts[1].toLongOrNull()
        val length = parts[2].toIntOrNull()
        if (relativePath.isEmpty() || offset == null || offset < 0 || length == null || length < 0) {
            return ERROR_JSON
        }

        return synchronized(taskLock) {
            val task = activeTask ?: return@synchronized ERROR_JSON
            val target = readableFileFor(relativePath) ?: return@synchronized ERROR_JSON
            if (!target.exists()) {
                return@synchronized "{\"type\":\"Error\",\"message\":\"file not found\"}"
            }
            val payload = readFileRange(target, offset, length)
            val encoded = Base64.encodeToString(payload, Base64.NO_WRAP)
            activeTask = task.copy(
                bytesTransferred = task.bytesTransferred + payload.size.toLong(),
                lastRelativePath = relativePath,
                updatedAtEpochMs = nowEpochMs(),
            )
            appendLog("transfer", activeTask?.taskId ?: "", "chunk read: $relativePath@$offset")
            "{\"type\":\"ChunkPayload\",\"relative_path\":\"${escapeJson(relativePath)}\",\"offset\":$offset,\"length\":${payload.size},\"payload\":\"$encoded\"}"
        }
    }

    private fun pullChunkBinary(args: String, output: OutputStream) {
        val parts = args.split(" ", limit = 3)
        if (parts.size != 3) {
            writeUtf8Line(output, ERROR_JSON)
            return
        }

        val relativePath = decodePathArg(parts[0]) ?: run {
            writeUtf8Line(output, ERROR_JSON)
            return
        }
        val offset = parts[1].toLongOrNull()
        val length = parts[2].toIntOrNull()
        if (relativePath.isEmpty() || offset == null || offset < 0 || length == null || length < 0) {
            writeUtf8Line(output, ERROR_JSON)
            return
        }

        val result = synchronized(taskLock) {
            val task = activeTask ?: return@synchronized null
            val target = readableFileFor(relativePath) ?: return@synchronized null
            if (!target.exists()) {
                return@synchronized null
            }
            val payload = readFileRange(target, offset, length)
            activeTask = task.copy(
                bytesTransferred = task.bytesTransferred + payload.size.toLong(),
                lastRelativePath = relativePath,
                updatedAtEpochMs = nowEpochMs(),
            )
            appendLog("transfer", activeTask?.taskId ?: "", "binary chunk read: $relativePath@$offset")
            val header = "{\"type\":\"ChunkPayloadBin\",\"relative_path\":\"${escapeJson(relativePath)}\",\"offset\":$offset,\"length\":${payload.size}}"
            Pair(header, payload)
        }

        if (result == null) {
            writeUtf8Line(output, ERROR_JSON)
            return
        }
        output.write(result.first.toByteArray(StandardCharsets.UTF_8))
        output.write('\n'.code)
        output.write(result.second)
        output.flush()
    }

    private fun readFileRange(target: File, offset: Long, length: Int): ByteArray {
        if (length == 0) {
            return ByteArray(0)
        }
        val payload = ByteArray(length)
        var totalRead = 0
        RandomAccessFile(target, "r").use { input ->
            input.seek(offset)
            while (totalRead < length) {
                val read = input.read(payload, totalRead, length - totalRead)
                if (read < 0) {
                    break
                }
                totalRead += read
            }
        }
        return payload.copyOf(totalRead)
    }

    private fun statFile(relativePath: String): String {
        val cleanPath = decodePathArg(relativePath.trim()) ?: return ERROR_JSON
        if (cleanPath.isEmpty()) {
            return ERROR_JSON
        }

        return synchronized(taskLock) {
            activeTask ?: return@synchronized ERROR_JSON
            val target = readableFileFor(cleanPath) ?: return@synchronized ERROR_JSON
            if (!target.exists()) {
                return@synchronized "{\"type\":\"Error\",\"message\":\"file not found\"}"
            }
            "{\"type\":\"FileStat\",\"relative_path\":\"${escapeJson(cleanPath)}\",\"size_bytes\":${target.length()}}"
        }
    }

    private fun verifyFile(args: String): String {
        val parts = args.split(" ", limit = 2)
        val relativePath = decodePathArg(parts.firstOrNull()?.trim().orEmpty()) ?: return ERROR_JSON
        val algorithm = parts.getOrNull(1)?.trim()?.ifEmpty { "BLAKE3" } ?: "BLAKE3"
        if (relativePath.isEmpty()) {
            return ERROR_JSON
        }

        val taskId: String
        val target: File
        synchronized(taskLock) {
            val task = activeTask ?: return ERROR_JSON
            val readable = readableFileFor(relativePath) ?: return ERROR_JSON
            if (!readable.exists()) {
                return "{\"type\":\"Error\",\"message\":\"file not found\"}"
            }
            if (!algorithm.equals("BLAKE3", ignoreCase = true)) {
                return "{\"type\":\"Error\",\"message\":\"unsupported verify algorithm: ${escapeJson(algorithm)}\"}"
            }
            taskId = task.taskId
            target = readable
            activeTask = task.copy(
                message = "verifying target file",
                lastRelativePath = relativePath,
                updatedAtEpochMs = nowEpochMs(),
            )
        }

        val digest = blake3Digest(target)
        synchronized(taskLock) {
            activeTask = activeTask?.copy(
                message = "verify completed",
                lastRelativePath = relativePath,
                updatedAtEpochMs = nowEpochMs(),
            )
            appendLog("transfer", taskId, "verify requested: $relativePath")
        }
        return "{\"type\":\"VerifyResult\",\"relative_path\":\"${escapeJson(relativePath)}\",\"algorithm\":\"BLAKE3\",\"digest\":\"$digest\"}"
    }

    private fun currentFileSnapshotJson(): String {
        return synchronized(taskLock) {
            activeTask ?: return@synchronized ERROR_JSON
            val file = currentFile ?: return@synchronized ERROR_JSON
            file.toJson("file snapshot")
        }
    }

    private fun currentTaskSnapshotJson(): String {
        return synchronized(taskLock) {
            activeTask?.toJson() ?: idleTaskSnapshotJson()
        }
    }

    private fun updateActiveTaskState(state: String): String {
        return synchronized(taskLock) {
            val snapshot = activeTask ?: return@synchronized idleTaskSnapshotJson()
            val updated = snapshot.copy(state = state, updatedAtEpochMs = nowEpochMs())
            activeTask = updated
            appendLog("audit", snapshot.taskId, "task state changed: $state")
            updated.toJson()
        }
    }

    private fun appendLog(scope: String, taskId: String, message: String) {
        val line = "{\"type\":\"LogRecord\",\"scope\":\"${escapeJson(scope)}\",\"task_id\":\"${escapeJson(taskId)}\",\"message\":\"${escapeJson(message)}\",\"epoch_ms\":${nowEpochMs()}}"
        logs.addLast(line)
        while (logs.size > MAX_IN_MEMORY_LOGS) {
            logs.removeFirst()
        }
        AgentServerStorage.root?.let { root ->
            runCatching {
                val logDir = File(root, "logs")
                logDir.mkdirs()
                val logFile = File(logDir, "agent.jsonl")
                rotateLogFileIfNeeded(logFile)
                logFile.appendText(line + "\n")
            }
        }
    }

    private fun writeUtf8Line(output: OutputStream, line: String) {
        output.write(line.toByteArray(StandardCharsets.UTF_8))
        output.write('\n'.code)
        output.flush()
    }

    private fun logSnapshot(): String {
        return synchronized(taskLock) {
            "{\"type\":\"LogBatch\",\"records\":[${logs.joinToString(",")}]}"
        }
    }

    data class BundleEntry(
        val relativePath: String,
        val sizeBytes: Long,
        val modifiedAtEpochMs: Long,
        val isDirectory: Boolean = false,
    )

    data class AgentTaskSummary(
        val taskId: String,
        val state: String,
        val message: String,
        val files: String,
        val chunks: String,
        val bytes: String,
        val lastPath: String,
    )

    private data class TaskSnapshot(
        val taskId: String,
        val state: String,
        val message: String,
        val filesStarted: Int = 0,
        val filesCompleted: Int = 0,
        val ackedChunks: Int = 0,
        val bytesTransferred: Long = 0,
        val lastRelativePath: String? = null,
        val updatedAtEpochMs: Long = nowEpochMs(),
    ) {
        fun toJson(): String {
            return buildString {
                append("{")
                append("\"type\":\"TaskSnapshot\",")
                append("\"task_id\":\"").append(escapeJson(taskId)).append("\",")
                append("\"state\":\"").append(escapeJson(state)).append("\",")
                append("\"message\":\"").append(escapeJson(message)).append("\",")
                append("\"files_started\":").append(filesStarted).append(",")
                append("\"files_completed\":").append(filesCompleted).append(",")
                append("\"acked_chunks\":").append(ackedChunks).append(",")
                append("\"bytes_transferred\":").append(bytesTransferred).append(",")
                append("\"last_relative_path\":")
                if (lastRelativePath == null) {
                    append("null")
                } else {
                    append("\"").append(escapeJson(lastRelativePath)).append("\"")
                }
                append(",")
                append("\"updated_at_epoch_ms\":").append(updatedAtEpochMs)
                append("}")
            }
        }
    }

    private class FileState(
        val taskId: String,
        val relativePath: String,
        val sizeBytes: Long,
        val ackedChunks: MutableSet<Int> = linkedSetOf(),
        var ackedBytes: Long = 0,
        var lastChunkIndex: Int? = null,
        var lastOffset: Long? = null,
        var lastLength: Int = 0,
        var completed: Boolean = false,
        var updatedAtEpochMs: Long = nowEpochMs(),
    ) {
        fun toJson(message: String): String {
            return buildString {
                append("{")
                append("\"type\":\"FileSnapshot\",")
                append("\"task_id\":\"").append(escapeJson(taskId)).append("\",")
                append("\"relative_path\":\"").append(escapeJson(relativePath)).append("\",")
                append("\"size_bytes\":").append(sizeBytes).append(",")
                append("\"acked_chunks\":").append(ackedChunks.size).append(",")
                append("\"acked_bytes\":").append(ackedBytes).append(",")
                append("\"last_chunk_index\":")
                append(lastChunkIndex ?: "null")
                append(",")
                append("\"last_offset\":")
                append(lastOffset ?: "null")
                append(",")
                append("\"last_length\":").append(lastLength).append(",")
                append("\"completed\":").append(completed).append(",")
                append("\"updated_at_epoch_ms\":").append(updatedAtEpochMs).append(",")
                append("\"message\":\"").append(escapeJson(message)).append("\"")
                append("}")
            }
        }
    }

    private fun chunkStatusJson(
        file: FileState,
        chunkIndex: Int,
        offset: Long?,
        length: Int,
        status: String,
        message: String,
    ): String {
        return chunkProbeJson(
            taskId = file.taskId,
            relativePath = file.relativePath,
            chunkIndex = chunkIndex,
            offset = offset,
            length = length,
            status = status,
            ackedChunks = file.ackedChunks.size,
            ackedBytes = file.ackedBytes,
            message = message,
        )
    }

    private fun chunkProbeJson(
        taskId: String,
        relativePath: String,
        chunkIndex: Int,
        offset: Long?,
        length: Int,
        status: String,
        ackedChunks: Int,
        ackedBytes: Long,
        message: String,
    ): String {
        return buildString {
            append("{")
            append("\"type\":\"ChunkAck\",")
            append("\"task_id\":\"").append(escapeJson(taskId)).append("\",")
            append("\"relative_path\":\"").append(escapeJson(relativePath)).append("\",")
            append("\"chunk_index\":").append(chunkIndex).append(",")
            append("\"offset\":")
            append(offset ?: "null")
            append(",")
            append("\"length\":").append(length).append(",")
            append("\"status\":\"").append(escapeJson(status)).append("\",")
            append("\"acked_chunks\":").append(ackedChunks).append(",")
            append("\"acked_bytes\":").append(ackedBytes).append(",")
            append("\"message\":\"").append(escapeJson(message)).append("\"")
            append("}")
        }
    }
}

private fun readCommandLine(input: InputStream): String? {
    val output = ByteArrayOutputStream()
    while (true) {
        val value = input.read()
        if (value < 0) {
            break
        }
        if (value == '\n'.code) {
            break
        }
        if (value != '\r'.code) {
            output.write(value)
        }
        if (output.size() > 16 * 1024) {
            return null
        }
    }
    if (output.size() == 0) {
        return null
    }
    return output.toString(StandardCharsets.UTF_8.name())
}

private fun readExactBytes(input: InputStream, length: Int): ByteArray {
    val payload = ByteArray(length)
    var offset = 0
    while (offset < length) {
        val read = input.read(payload, offset, length - offset)
        if (read < 0) {
            throw java.io.EOFException("expected $length bytes, got $offset")
        }
        offset += read
    }
    return payload
}

private fun copyExactBytes(
    input: InputStream,
    output: RandomAccessFile,
    length: Int,
    buffer: ByteArray,
): Boolean {
    var remaining = length
    while (remaining > 0) {
        val wanted = minOf(buffer.size, remaining)
        val read = input.read(buffer, 0, wanted)
        if (read < 0) {
            return false
        }
        output.write(buffer, 0, read)
        remaining -= read
    }
    return true
}

private fun readNetworkInt(input: InputStream): Int {
    val bytes = readExactBytes(input, 4)
    return ((bytes[0].toInt() and 0xff) shl 24) or
        ((bytes[1].toInt() and 0xff) shl 16) or
        ((bytes[2].toInt() and 0xff) shl 8) or
        (bytes[3].toInt() and 0xff)
}

private fun readNetworkLong(input: InputStream): Long {
    val bytes = readExactBytes(input, 8)
    var value = 0L
    for (byte in bytes) {
        value = (value shl 8) or (byte.toLong() and 0xff)
    }
    return value
}

private fun rotateLogFileIfNeeded(currentFile: File) {
    if (!currentFile.exists() || currentFile.length() < MAX_LOG_FILE_BYTES) {
        return
    }

    val oldestArchive = File(currentFile.parentFile, "agent.${MAX_LOG_ARCHIVES}.jsonl")
    if (oldestArchive.exists()) {
        oldestArchive.delete()
    }

    for (index in MAX_LOG_ARCHIVES - 1 downTo 1) {
        val source = File(currentFile.parentFile, "agent.$index.jsonl")
        if (!source.exists()) {
            continue
        }
        val destination = File(currentFile.parentFile, "agent.${index + 1}.jsonl")
        source.renameTo(destination)
    }

    currentFile.renameTo(File(currentFile.parentFile, "agent.1.jsonl"))
}

private fun formatBytes(value: Long): String {
    val units = arrayOf("B", "KB", "MB", "GB")
    var unitIndex = 0
    var size = value.toDouble()
    while (size >= 1024.0 && unitIndex < units.lastIndex) {
        size /= 1024.0
        unitIndex += 1
    }
    return if (unitIndex == 0) {
        "${value} B"
    } else {
        "%.1f %s".format(size, units[unitIndex])
    }
}

private fun blake3Digest(file: File): String {
    val hasher = Blake3.newInstance()
    val buffer = ByteArray(VERIFY_BUFFER_BYTES)
    file.inputStream().use { input ->
        while (true) {
            val read = input.read(buffer)
            if (read < 0) {
                break
            }
            if (read == buffer.size) {
                hasher.update(buffer)
            } else if (read > 0) {
                hasher.update(buffer.copyOf(read))
            }
        }
    }
    return hasher.hexdigest()
}

private fun tempFileFor(relativePath: String): File? {
    return targetFileFor(relativePath, appendTempSuffix = true)
}

private fun finalFileFor(relativePath: String): File? {
    return targetFileFor(relativePath, appendTempSuffix = false)
}

private fun readableFileFor(relativePath: String): File? {
    val final = finalFileFor(relativePath) ?: return null
    if (final.exists()) {
        return final
    }
    return tempFileFor(relativePath)
}

private fun targetFileFor(relativePath: String, appendTempSuffix: Boolean): File? {
    val root = AgentServer.activeRoot() ?: return null
    val safeRelative = sanitizeRelativePath(relativePath)
    if (safeRelative.isEmpty()) {
        return null
    }

    val target = File(root, if (appendTempSuffix) "$safeRelative.nekotrans-tmp" else safeRelative)
    val rootPath = root.canonicalPath
    val targetPath = target.canonicalPath
    val allowedPrefix = rootPath.trimEnd(File.separatorChar) + File.separator
    return if (targetPath == rootPath || targetPath.startsWith(allowedPrefix)) {
        target
    } else {
        null
    }
}

private fun resolveTargetRoot(targetRoot: String): File? {
    val fallback = AgentServerStorage.root ?: return null
    val normalized = targetRoot.replace('\\', '/').trim()
    val root = if (normalized.startsWith("/")) {
        File(normalized)
    } else {
        File(fallback, sanitizeRelativePath(normalized))
    }
    val canonical = root.canonicalFile
    val fallbackCanonical = fallback.canonicalFile
    if (canonical.path == fallbackCanonical.path || canonical.path.startsWith(fallbackCanonical.path + File.separator)) {
        return canonical
    }
    return if (android.os.Environment.isExternalStorageManager()) {
        canonical
    } else {
        null
    }
}

private object AgentServerStorage {
    var root: File? = null
}

private fun idleTaskSnapshotJson(): String {
    return "{\"type\":\"TaskSnapshot\",\"task_id\":null,\"state\":\"Idle\",\"message\":\"no active task\",\"files_started\":0,\"files_completed\":0,\"acked_chunks\":0,\"bytes_transferred\":0,\"last_relative_path\":null,\"updated_at_epoch_ms\":${nowEpochMs()}}"
}

private fun sanitizeRelativePath(input: String): String {
    return input
        .replace('\\', '/')
        .split('/')
        .filter { segment -> segment.isNotEmpty() && segment != "." && segment != ".." }
        .map { segment -> segment.filter { character -> !character.isISOControl() && character != '\u0000' } }
        .filter { segment -> segment.isNotEmpty() }
        .joinToString("/")
}

private fun decodePathArg(input: String): String? {
    val bytes = ByteArrayOutputStream(input.length)
    var index = 0
    while (index < input.length) {
        val character = input[index]
        if (character == '%') {
            if (index + 2 >= input.length) {
                return null
            }
            val high = input[index + 1].digitToIntOrNull(16) ?: return null
            val low = input[index + 2].digitToIntOrNull(16) ?: return null
            bytes.write((high shl 4) or low)
            index += 3
        } else {
            bytes.write(character.toString().toByteArray(StandardCharsets.UTF_8))
            index += 1
        }
    }
    return bytes.toByteArray().toString(StandardCharsets.UTF_8)
}

private fun parseBundleManifest(input: String): List<AgentServer.BundleEntry>? {
    val entries = mutableListOf<AgentServer.BundleEntry>()
    for (line in input.lineSequence()) {
        if (line.isBlank()) {
            continue
        }
        val parts = line.split('\t')
        when (parts.firstOrNull()) {
            "F" -> {
                if (parts.size != 4) {
                    return null
                }
                val relativePath = decodePathArg(parts[1]) ?: return null
                val sizeBytes = parts[2].toLongOrNull() ?: return null
                val modifiedAtEpochMs = parts[3].toLongOrNull() ?: return null
                if (relativePath.isEmpty() || sizeBytes < 0) {
                    return null
                }
                entries += AgentServer.BundleEntry(relativePath, sizeBytes, modifiedAtEpochMs)
            }
            "D" -> {
                if (parts.size != 2) {
                    return null
                }
                val relativePath = decodePathArg(parts[1]) ?: return null
                if (relativePath.isEmpty()) {
                    return null
                }
                entries += AgentServer.BundleEntry(
                    relativePath = relativePath,
                    sizeBytes = 0,
                    modifiedAtEpochMs = 0,
                    isDirectory = true,
                )
            }
            else -> return null
        }
    }
    return entries
}

private fun escapeJson(input: String): String {
    return input
        .replace("\\", "\\\\")
        .replace("\"", "\\\"")
        .replace("\n", "\\n")
        .replace("\r", "\\r")
        .replace("\t", "\\t")
}

private fun nowEpochMs(): Long = System.currentTimeMillis()
