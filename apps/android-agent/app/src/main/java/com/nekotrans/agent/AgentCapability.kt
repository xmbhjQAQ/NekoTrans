package com.nekotrans.agent

import android.os.Environment

data class AgentCapability(
    val supportsAdb: Boolean,
    val supportsWifi: Boolean,
    val supportsDual: Boolean,
    val storagePermissionState: String,
    val appVersion: String,
    val protocolVersion: String,
    val mode: String,
) {
    fun toJson(): String {
        return buildString {
            append("{")
            append("\"type\":\"Capability\",")
            append("\"supports_adb\":").append(supportsAdb).append(",")
            append("\"supports_wifi\":").append(supportsWifi).append(",")
            append("\"supports_dual\":").append(supportsDual).append(",")
            append("\"storage_permission_state\":\"").append(escapeJson(storagePermissionState)).append("\",")
            append("\"app_version\":\"").append(escapeJson(appVersion)).append("\",")
            append("\"protocol_version\":\"").append(escapeJson(protocolVersion)).append("\",")
            append("\"mode\":\"").append(escapeJson(mode)).append("\"")
            append("}")
        }
    }

    companion object {
        fun current(): AgentCapability {
            return AgentCapability(
                supportsAdb = true,
                supportsWifi = true,
                supportsDual = false,
                storagePermissionState = if (Environment.isExternalStorageManager()) {
                    "granted"
                } else {
                    "required"
                },
                appVersion = "0.1.0",
                protocolVersion = "0.1",
                mode = "capability-only",
            )
        }
    }
}

private fun escapeJson(input: String): String {
    return input
        .replace("\\", "\\\\")
        .replace("\"", "\\\"")
        .replace("\n", "\\n")
        .replace("\r", "\\r")
        .replace("\t", "\\t")
}
