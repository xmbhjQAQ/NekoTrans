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
                            text = "Keep this screen or the foreground notification active while the desktop sends tasks.",
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
                                        "Foreground Service Running"
                                    } else {
                                        "Start Foreground Service"
                                    },
                                )
                            }

                            Button(
                                modifier = Modifier.fillMaxWidth(),
                                onClick = { uiState = readUiState() },
                            ) {
                                Text("Refresh Status")
                            }

                            Button(
                                modifier = Modifier.fillMaxWidth(),
                                onClick = {
                                    startActivity(
                                        Intent(Settings.ACTION_MANAGE_ALL_FILES_ACCESS_PERMISSION),
                                    )
                                },
                            ) {
                                Text("Grant File Access")
                            }

                            Button(
                                modifier = Modifier.fillMaxWidth(),
                                onClick = {
                                    openBatteryOptimizationSettings()
                                    uiState = readUiState()
                                },
                            ) {
                                Text("Allow Screen-Off Network")
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
            endpointText = if (ip == "Unavailable") "Waiting for Wi-Fi" else "$ip:${AgentServer.PORT}",
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
            Text("Desktop pairing", fontWeight = FontWeight.Bold)
            SelectionContainer {
                Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
                    StatusRow("Agent Host", state.lanAddress)
                    StatusRow("Desktop entry", state.endpointText)
                    StatusRow("Service", state.serviceText)
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
            Text("Current task", fontWeight = FontWeight.Bold)
            StatusRow("Task ID", task.taskId)
            StatusRow("State", task.state)
            StatusRow("Files", task.files)
            StatusRow("Chunks", task.chunks)
            StatusRow("Bytes", task.bytes)
            StatusRow("Last path", task.lastPath)
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
            Text("Runtime readiness", fontWeight = FontWeight.Bold)
            StatusRow("All Files Access", if (state.allFilesAccess) "Granted" else "Required")
            StatusRow(
                "Battery Optimization",
                if (state.batteryExempt) "Exempt" else "May restrict screen-off LAN",
            )
            StatusRow("Wake/Wi-Fi Locks", if (state.running) "Held by service" else "Not held")
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
