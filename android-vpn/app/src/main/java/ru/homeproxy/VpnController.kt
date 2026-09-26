package ru.homeproxy

import android.content.Context

/**
 * Включение и выключение своего VPN ([HpVpnService]).
 *
 * Порядок работы: сначала поднимается клиент (`HomeProxy.start`): дыры к роутеру уже открыты
 * и по ним идёт keep-alive. Только после этого можно включать VPN.
 */
object VpnController {
    @Volatile var isUp: Boolean = false
        internal set

    /** Итог последнего включения (null — успех или ещё не включали). */
    @Volatile var lastMessage: String? = null

    /** Можно ли сейчас включать VPN: хотя бы одна дыра к роутеру уже живая. */
    fun canStart(): Boolean = HomeProxy.liveHoles() >= 1

    /** Включает VPN (согласие `VpnService.prepare` уже должно быть получено). */
    fun start(context: Context) {
        lastMessage = "VPN включается…"
        context.startService(HpVpnService.intent(context, HpVpnService.ACTION_CONNECT))
    }

    /** Выключает VPN (клиент и дыры продолжают работать). */
    fun stop(context: Context) {
        if (isUp) context.startService(HpVpnService.intent(context, HpVpnService.ACTION_DISCONNECT))
    }
}
