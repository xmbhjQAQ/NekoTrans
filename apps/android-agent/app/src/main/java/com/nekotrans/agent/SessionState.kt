package com.nekotrans.agent

data class SessionState(
    val sessionId: String,
    val taskId: String,
    val lane: String,
    val filePath: String,
    val offset: Long,
    val length: Long,
    val paused: Boolean
)
