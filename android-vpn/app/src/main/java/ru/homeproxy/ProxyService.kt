package ru.homeproxy

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.util.Log
import kotlin.concurrent.thread

/**
 * Держит клиента живым, пока приложение свёрнуто: foreground-сервис с
 * постоянным уведомлением. Запуск и остановка нативного клиента идут в
 * отдельном потоке, чтобы не блокировать главный.
 */
class ProxyService : Service() {
    private var statusThread: Thread? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_STOP -> {
                thread(name = "homeproxy-stop") {
                    HomeProxy.stop()
                    stopForeground(STOP_FOREGROUND_REMOVE)
                    stopSelf()
                }
            }
            else -> {
                startForegroundCompat()
                thread(name = "homeproxy-start") {
                    val error = runCatching { HomeProxy.start(Settings(this).toConfig()) }
                        .getOrElse { it.message ?: it.toString() }
                    if (error != null) {
                        Log.w(TAG, "запуск не удался: $error")
                        lastError = error
                        stopForeground(STOP_FOREGROUND_REMOVE)
                        stopSelf()
                    } else {
                        lastError = null
                        startStatusLoop()
                    }
                }
            }
        }
        return START_NOT_STICKY
    }

    /** Раз в 5 секунд пишет состояние в logcat (тег homeproxy) и в уведомление, если оно изменилось. */
    private fun startStatusLoop() {
        statusThread?.interrupt()
        statusThread = thread(name = "homeproxy-status") {
            var last = ""
            try {
                while (true) {
                    val status = HomeProxy.status()
                    if (status != last) {
                        last = status
                        Log.i(TAG, "состояние: $status")
                        getSystemService(NotificationManager::class.java)
                            .notify(NOTIFICATION_ID, notification(status))
                    }
                    Thread.sleep(5000)
                }
            } catch (_: InterruptedException) {
                // сервис остановлен
            }
        }
    }

    override fun onDestroy() {
        statusThread?.interrupt()
        HomeProxy.stop()
        super.onDestroy()
    }

    private fun notification(text: String): Notification =
        Notification.Builder(this, CHANNEL_ID)
            .setContentTitle("HomeProxy")
            .setContentText(text)
            .setSmallIcon(android.R.drawable.stat_sys_upload)
            .setOngoing(true)
            .build()

    private fun startForegroundCompat() {
        val manager = getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(CHANNEL_ID, "HomeProxy", NotificationManager.IMPORTANCE_LOW),
        )
        val notification = notification("Клиент запускается")
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(NOTIFICATION_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }
    }

    companion object {
        private const val TAG = "homeproxy"
        private const val CHANNEL_ID = "homeproxy"
        private const val NOTIFICATION_ID = 1
        const val ACTION_STOP = "ru.homeproxy.STOP"

        /** Текст последней ошибки запуска (для показа на экране). */
        @Volatile var lastError: String? = null

        fun start(context: Context) {
            context.startForegroundService(Intent(context, ProxyService::class.java))
        }

        fun stop(context: Context) {
            context.startService(Intent(context, ProxyService::class.java).setAction(ACTION_STOP))
        }
    }
}
