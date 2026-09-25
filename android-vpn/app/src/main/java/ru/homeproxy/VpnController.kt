package ru.homeproxy

import android.content.Context
import android.util.Log
import com.wireguard.android.backend.GoBackend
import com.wireguard.android.backend.Tunnel
import com.wireguard.config.Config
import java.io.ByteArrayInputStream

/**
 * VPN на WireGuard (официальная библиотека, GoBackend со своим VpnService).
 *
 * Порядок работы: сначала поднимается клиент (`HomeProxy.start`): дыры к роутеру
 * уже открыты и по ним идёт keep-alive. Только после этого можно включать VPN:
 * его endpoint — локальный мост клиента (см. [WgConfig]).
 */
object VpnController {
    private const val TAG = "homeproxy"
    private const val TUNNEL_NAME = "homeproxy"

    @Volatile private var backend: GoBackend? = null

    @Volatile var isUp: Boolean = false
        private set

    private val tunnel = object : Tunnel {
        override fun getName(): String = TUNNEL_NAME

        override fun onStateChange(newState: Tunnel.State) {
            isUp = newState == Tunnel.State.UP
            Log.i(TAG, "VPN: $newState")
        }
    }

    @Synchronized
    private fun backend(context: Context): GoBackend =
        backend ?: GoBackend(context.applicationContext).also { backend = it }

    /** Можно ли сейчас включать VPN: хотя бы одна дыра к роутеру уже живая. */
    fun canStart(): Boolean = HomeProxy.liveHoles() >= 1

    /**
     * Включает VPN. Блокирует поток (вызывать не из главного) и возвращает null при
     * успехе либо текст ошибки. Требует, чтобы согласие на VPN уже было получено
     * (`VpnService.prepare`) и дыры были подняты.
     */
    fun start(context: Context, wgConfig: String, localPort: Int): String? {
        if (!canStart()) return "дыры ещё не подняты: сначала «Подключить» и дождитесь живых дыр"
        Log.i(TAG, "VPN: запускаем, живых дыр к роутеру: ${HomeProxy.liveHoles()}")
        val config = try {
            val prepared = WgConfig.prepare(wgConfig, localPort, context.packageName)
            Config.parse(ByteArrayInputStream(prepared.toByteArray()))
        } catch (e: Exception) {
            return "конфиг WireGuard: ${e.message}"
        }
        return try {
            backend(context).setState(tunnel, Tunnel.State.UP, config)
            null
        } catch (e: Exception) {
            Log.w(TAG, "VPN не включился", e)
            e.message ?: e.toString()
        }
    }

    /** Выключает VPN (клиент и дыры продолжают работать). */
    fun stop(context: Context): String? = try {
        backend(context).setState(tunnel, Tunnel.State.DOWN, null)
        null
    } catch (e: Exception) {
        e.message ?: e.toString()
    }
}
