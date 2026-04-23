package com.nekotrans.agent

import android.Manifest
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Environment
import android.os.PowerManager
import android.provider.Settings
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import java.net.Inet4Address
import java.net.NetworkInterface

class MainActivity : ComponentActivity() {
    private val notificationPermission = registerForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            notificationPermission.launch(Manifest.permission.POST_NOTIFICATIONS)
        }
        startTransferService()

        setContent {
            var uiState by remember { mutableStateOf(readUiState()) }
            MaterialTheme {
                Surface(
                    modifier = Modifier
                        .fillMaxSize()
                        .background(
                            brush = Brush.linearGradient(
                                listOf(Color(0xFF071219), Color(0xFF102732)),
                            ),
                        ),
                ) {
                    Column(
                        modifier = Modifier
                            .fillMaxSize()
                            .verticalScroll(rememberScrollState())
                            .padding(24.dp),
                        verticalArrangement = Arrangement.spacedBy(16.dp),
                    ) {
                        Text(
                            text = "Nekotrans Agent",
                            color = Color.White,
                            style = MaterialTheme.typography.headlineMedium,
                        )
                        Text(
                            text = "保持前台服务运行，桌面端即可通过 ADB 与 Wi-Fi 下发传输任务。",
                            color = Color(0xFFB6D2DB),
                        )

                        PairingCard(uiState)
                        TaskCard(uiState.task)
                        PermissionCard(uiState)

                        Column(
                            modifier = Modifier.fillMaxWidth(),
                            verticalArrangement = Arrangement.spacedBy(12.dp),
                        ) {
                            Button(
                                modifier = Modifier.fillMaxWidth(),
                                onClick = {
                                    startTransferService()
                                    uiState = readUiState()
                                },
                            ) {
                                Text(
                                    if (uiState.running) {
                                        "前台服务运行中"
                                    } else {
                                        "启动前台服务"
                                    },
                                )
                            }

                            Button(
                                modifier = Modifier.fillMaxWidth(),
                                onClick = { uiState = readUiState() },
                            ) {
                                Text("刷新状态")
                            }

                            Button(
                                modifier = Modifier.fillMaxWidth(),
                                onClick = {
                                    startActivity(
                                        Intent(Settings.ACTION_MANAGE_ALL_FILES_ACCESS_PERMISSION),
                                    )
                                },
                            ) {
                                Text("授予文件访问")
                            }

                            Button(
                                modifier = Modifier.fillMaxWidth(),
                                onClick = {
                                    openBatteryOptimizationSettings()
                                    uiState = readUiState()
                                },
                            ) {
                                Text("允许熄屏网络")
                            }
                        }
                    }
                }
            }
        }
    }

    private fun readUiState(): AgentUiState {
        val ip = localLanAddress() ?: "Unavailable"
        return AgentUiState(
            running = AgentServer.isRunning(),
            serviceText = AgentServer.statusText(),
            lanAddress = ip,
            endpointText = if (ip == "Unavailable") "等待 Wi-Fi" else "$ip:${AgentServer.PORT}",
            allFilesAccess = Environment.isExternalStorageManager(),
            batteryExempt = isIgnoringBatteryOptimizations(),
            task = AgentServer.taskSummary(),
        )
    }

    private fun isIgnoringBatteryOptimizations(): Boolean {
        val powerManager = getSystemService(PowerManager::class.java)
        return powerManager.isIgnoringBatteryOptimizations(packageName)
    }

    private fun openBatteryOptimizationSettings() {
        val requestIntent = Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS).apply {
            data = Uri.parse("package:$packageName")
        }
        val fallbackIntent = Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS)
        runCatching {
            startActivity(requestIntent)
        }.onFailure {
            startActivity(fallbackIntent)
        }
    }

    private fun startTransferService() {
        val intent = Intent(this, TransferService::class.java)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            startForegroundService(intent)
        } else {
            startService(intent)
        }
    }
}

private data class AgentUiState(
    val running: Boolean,
    val serviceText: String,
    val lanAddress: String,
    val endpointText: String,
    val allFilesAccess: Boolean,
    val batteryExempt: Boolean,
    val task: AgentServer.AgentTaskSummary,
)

@Composable
private fun PairingCard(state: AgentUiState) {
    Card(
        shape = RoundedCornerShape(12.dp),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(
            modifier = Modifier.padding(18.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Text("桌面端配对", fontWeight = FontWeight.Bold)
            SelectionContainer {
                Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
                    StatusRow("代理地址", state.lanAddress)
                    StatusRow("桌面端填写", state.endpointText)
                    StatusRow("服务", state.serviceText)
                }
            }
        }
    }
}

@Composable
private fun TaskCard(task: AgentServer.AgentTaskSummary) {
    Card(
        shape = RoundedCornerShape(12.dp),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(
            modifier = Modifier.padding(18.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Text("当前任务", fontWeight = FontWeight.Bold)
            StatusRow("任务 ID", task.taskId)
            StatusRow("状态", displayTaskState(task.state))
            StatusRow("文件", task.files)
            StatusRow("分块", task.chunks)
            StatusRow("字节", task.bytes)
            StatusRow("最近路径", task.lastPath)
            Text(task.message, color = Color(0xFF5D7480))
        }
    }
}

@Composable
private fun PermissionCard(state: AgentUiState) {
    Card(
        shape = RoundedCornerShape(12.dp),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(
            modifier = Modifier.padding(18.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Text("运行准备", fontWeight = FontWeight.Bold)
            StatusRow("所有文件访问", if (state.allFilesAccess) "已授权" else "需要授权")
            StatusRow(
                "电池优化",
                if (state.batteryExempt) "已放行" else "可能限制熄屏网络",
            )
            StatusRow("唤醒 / Wi-Fi 锁", if (state.running) "服务已持有" else "未持有")
        }
    }
}

@Composable
private fun StatusRow(label: String, value: String) {
    Row(
        modifier = Modifier.fillMaxWidth(),
        horizontalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        Text(
            text = label,
            modifier = Modifier.weight(0.42f),
            color = Color(0xFF5D7480),
        )
        Text(
            text = value,
            modifier = Modifier.weight(0.58f),
            color = Color(0xFF2A8FFF),
        )
    }
}

private fun localLanAddress(): String? {
    return runCatching {
        NetworkInterface.getNetworkInterfaces().toList()
            .filter { networkInterface -> networkInterface.isUp && !networkInterface.isLoopback }
            .flatMap { networkInterface -> networkInterface.inetAddresses.toList() }
            .filterIsInstance<Inet4Address>()
            .map { address -> address.hostAddress }
            .filterNotNull()
            .firstOrNull { address ->
                address.startsWith("192.168.") ||
                    address.startsWith("10.") ||
                    address.matches(Regex("""172\.(1[6-9]|2\d|3[0-1])\..*"""))
            }
    }.getOrNull()
}

private fun displayTaskState(state: String): String {
    return when (state) {
        "Idle" -> "空闲"
        "Running" -> "运行中"
        "Paused" -> "已暂停"
        "Cancelled" -> "已取消"
        "Completed" -> "已完成"
        else -> state
    }
}
