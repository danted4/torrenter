package com.danted4.torrenter

import android.app.Application
import android.app.NotificationChannel
import android.app.NotificationManager
import uniffi.torrenter.Engine
import uniffi.torrenter.debridServices

class TorrenterApp : Application() {
    private val lock = Any()
    private val notices = ArrayList<String>()
    private var ticker: Thread? = null

    @Volatile
    private var stopTicker = false

    var engine: Engine? = null
        private set

    override fun onCreate() {
        super.onCreate()
        val manager = getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(
                DownloadService.CHANNEL_ID,
                "downloads",
                NotificationManager.IMPORTANCE_LOW,
            ),
        )
    }

    fun openEngine(seed: Boolean): Engine =
        synchronized(lock) {
            engine?.let {
                return it
            }
            val dest = Paths.publicDest()
            dest.mkdirs()
            val created =
                Engine.open(
                    dest.absolutePath,
                    Paths.jobsFile(filesDir).absolutePath,
                    seed,
                )
            // Keys go in before hydrate so restored debrid jobs can start.
            for (service in debridServices()) {
                SecureKeys.read(this, service.id)?.let { created.setDebridKey(service.id, it) }
            }
            val prefs = getSharedPreferences(Prefs.NAME, MODE_PRIVATE)
            created.setConnections(Prefs.connectionsArg(Prefs.connections(prefs)))
            created.setDebridRemoveAfter(Prefs.removeAfter(prefs))
            created.hydrate()
            engine = created
            startTicker()
            created
        }

    fun takeNotices(): List<String> =
        synchronized(lock) {
            if (notices.isEmpty()) {
                emptyList()
            } else {
                notices.toList().also { notices.clear() }
            }
        }

    fun tick(): Boolean =
        synchronized(lock) {
            val live = engine ?: return false
            notices += live.drainNotices()
            val active = live.anyActive()
            live.persist(!active)
            active
        }

    private fun startTicker() {
        if (ticker != null) {
            return
        }
        stopTicker = false
        ticker =
            Thread(
                {
                    while (!stopTicker) {
                        val active =
                            try {
                                tick()
                            } catch (_: Exception) {
                                false
                            }
                        DownloadService.syncFromOwner(this, active)
                        try {
                            Thread.sleep(TICK_MS)
                        } catch (_: InterruptedException) {
                            break
                        }
                    }
                },
                "torrenter-session",
            ).apply {
                isDaemon = true
                start()
            }
    }

    companion object {
        const val TICK_MS = 200L
    }
}
