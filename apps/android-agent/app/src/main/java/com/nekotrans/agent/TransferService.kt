package com.nekotrans.agent

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.net.wifi.WifiManager
import android.os.Build
import android.os.IBinder
import android.os.PowerManager

class TransferService : Service() {
    private var wakeLock: PowerManager.WakeLock? = null
    private var wifiLock: WifiManager.WifiLock? = null

    override fun onCreate() {
        super.onCreate()
        createChannel()
        acquireTransferLocks()
        AgentServer.start(this)
        startForeground(1001, buildNotification(getString(R.string.service_waiting)))
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        acquireTransferLocks()
        AgentServer.start(this)
        startForeground(1001, buildNotification(AgentServer.statusText()))
        return START_STICKY
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onDestroy() {
        AgentServer.stop()
        releaseTransferLocks()
        super.onDestroy()
    }

    @Suppress("DEPRECATION")
    private fun acquireTransferLocks() {
        runCatching {
            val powerManager = getSystemService(PowerManager::class.java)
            if (wakeLock == null) {
                wakeLock = powerManager.newWakeLock(
                    PowerManager.PARTIAL_WAKE_LOCK,
                    "Nekotrans:TransferWakeLock",
                ).apply {
                    setReferenceCounted(false)
                }
            }
            wakeLock?.takeUnless { it.isHeld }?.acquire()
        }

        runCatching {
            val wifiManager = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
            if (wifiLock == null) {
                wifiLock = wifiManager.createWifiLock(
                    WifiManager.WIFI_MODE_FULL_HIGH_PERF,
                    "Nekotrans:TransferWifiLock",
                ).apply {
                    setReferenceCounted(false)
                }
            }
            wifiLock?.takeUnless { it.isHeld }?.acquire()
        }
    }

    private fun releaseTransferLocks() {
        wifiLock?.takeIf { it.isHeld }?.release()
        wifiLock = null

        wakeLock?.takeIf { it.isHeld }?.release()
        wakeLock = null
    }

    private fun createChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val manager = getSystemService(NotificationManager::class.java)
            val channel = NotificationChannel(
                "nekotrans-agent",
                getString(R.string.channel_name),
                NotificationManager.IMPORTANCE_LOW,
            ).apply {
                description = getString(R.string.channel_description)
            }
            manager.createNotificationChannel(channel)
        }
    }

    @Suppress("DEPRECATION")
    private fun buildNotification(content: String): Notification {
        val builder = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            Notification.Builder(this, "nekotrans-agent")
        } else {
            Notification.Builder(this)
        }

        return builder
            .setContentTitle(getString(R.string.agent_title))
            .setContentText(content)
            .setSmallIcon(android.R.drawable.stat_sys_upload)
            .setOngoing(true)
            .build()
    }
}
