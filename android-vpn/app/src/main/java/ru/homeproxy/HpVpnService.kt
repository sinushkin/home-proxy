package ru.homeproxy

import android.content.Context
import android.content.Intent
import android.net.VpnService
import android.util.Log

/**
 * Свой VPN без WireGuard: интерфейс TUN с адресом телефона в туннеле, маршрут на всё и
 * DNS; дескриптор отдаётся клиенту ([HomeProxy.attachTun]), IP-пакеты идут по дырам как есть.
 *
 * Само приложение из VPN исключено (`addDisallowedApplication`): его сокеты — дыры, STUN,
 * MQTT — должны ходить напрямую, иначе дыры ушли бы в свой же туннель.
 */
class HpVpnService : VpnService() {
    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_DISCONNECT -> disconnect()
            else -> VpnController.lastMessage = connect()
        }
        return START_NOT_STICKY
    }

    /** Поднимает интерфейс и отдаёт его клиенту; null — успех, иначе текст ошибки. */
    private fun connect(): String? {
        if (HomeProxy.liveHoles() < 1) return "дыры ещё не подняты: сначала «Подключить»"
        val settings = Settings(this)
        val pfd = try {
            Builder()
                .setSession("HomeProxy")
                .addAddress(settings.tunAddr, 32)
                .addRoute("0.0.0.0", 0)
                .addDnsServer(settings.dns)
                .setMtu(MTU)
                .addDisallowedApplication(packageName)
                .establish()
        } catch (e: Exception) {
            Log.w(TAG, "VPN не поднялся", e)
            return "VPN: ${e.message ?: e}"
        } ?: return "VPN: нет согласия пользователя (VpnService.prepare)"
        val error = HomeProxy.attachTun(pfd.detachFd())
        if (error != null) return "TUN: $error"
        VpnController.isUp = true
        Log.i(TAG, "VPN включён: ${settings.tunAddr}, DNS ${settings.dns}")
        return null
    }

    private fun disconnect() {
        HomeProxy.detachTun()
        VpnController.isUp = false
        Log.i(TAG, "VPN выключен")
        stopSelf()
    }

    override fun onRevoke() {
        // Пользователь выключил VPN в системе или включил другой.
        disconnect()
        super.onRevoke()
    }

    override fun onDestroy() {
        if (VpnController.isUp) disconnect()
        super.onDestroy()
    }

    companion object {
        private const val TAG = "homeproxy"
        /** MTU туннеля: IP-пакет целиком едет в одной датаграмме дыры (с заголовками и подписью). */
        const val MTU = 1400
        const val ACTION_CONNECT = "ru.homeproxy.VPN_CONNECT"
        const val ACTION_DISCONNECT = "ru.homeproxy.VPN_DISCONNECT"

        fun intent(context: Context, action: String) = Intent(context, HpVpnService::class.java).setAction(action)
    }
}
