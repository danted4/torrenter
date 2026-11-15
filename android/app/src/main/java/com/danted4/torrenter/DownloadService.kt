package com.danted4.torrenter

import android.app.Notification
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.os.SystemClock
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat

class DownloadService : Service() {
    override fun onBind(intent: Intent?): IBinder? = null

    override fun onDestroy() {
        // Stopped (idle, or by the system): the next live tick starts it again.
        lastStart = 0L
        super.onDestroy()
    }

    override fun onStartCommand(
        intent: Intent?,
        flags: Int,
        startId: Int,
    ): Int {
        val notification = buildNotification()
        if (Build.VERSION.SDK_INT >= 29) {
            ServiceCompat.startForeground(
                this,
                NOTIFICATION_ID,
                notification,
                ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC,
            )
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }
        val app = application as TorrenterApp
        if (app.engine == null) {
            stopSelf()
        }
        return START_NOT_STICKY
    }

    private fun buildNotification(): Notification {
        val launch =
            PendingIntent.getActivity(
                this,
                0,
                Intent(this, MainActivity::class.java),
                PendingIntent.FLAG_IMMUTABLE,
            )
        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle("torrenter")
            .setContentText("Downloading / seeding")
            .setSmallIcon(R.drawable.ic_stat_magnet)
            .setContentIntent(launch)
            .setOngoing(true)
            .build()
    }

    companion object {
        const val CHANNEL_ID = "torrenter-jobs"
        const val NOTIFICATION_ID = 7

        /** When the UI last started the service (elapsed realtime, ms). */
        @Volatile
        private var lastStart = 0L

        /** Re-assert the service at most this often while jobs run. */
        private const val RESTART_EVERY_MS = 10_000L

        /**
         * Start the foreground service for live jobs. The UI ticks every
         * 200 ms; each start re-posts the notification, so start once and
         * then re-check only every [RESTART_EVERY_MS] (a stopped service
         * resets this and starts on the next tick).
         */
        fun startFromUi(context: Context) {
            val now = SystemClock.elapsedRealtime()
            if (lastStart != 0L && now - lastStart < RESTART_EVERY_MS) {
                return
            }
            // Count only a start that went through: Android 12+ refuses one
            // while the app is in the background, and the next tick retries.
            val started =
                runCatching {
                    context.startForegroundService(Intent(context, DownloadService::class.java))
                }.isSuccess
            if (started) {
                lastStart = now
            }
        }

        fun syncFromOwner(
            context: Context,
            active: Boolean,
        ) {
            if (active) {
                return
            }
            lastStart = 0L
            context.stopService(Intent(context, DownloadService::class.java))
        }
    }
}
