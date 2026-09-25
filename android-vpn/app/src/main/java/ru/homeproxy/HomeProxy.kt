package ru.homeproxy

/**
 * Обёртка над libhomeproxy.so (Rust, hp-backend/android-lib). Имена native-методов
 * связаны с именем пакета: Java_ru_homeproxy_HomeProxy_*.
 *
 * Клиент держит 10 UDP-дыр к роутеру и локальный UDP-мост на 127.0.0.1: WireGuard
 * настраивается так, что его endpoint — этот адрес.
 */
object HomeProxy {
    init {
        System.loadLibrary("homeproxy")
    }

    @JvmStatic private external fun nativeStart(
        stun: String, mqtt: String, caPem: String, myId: String, peerId: String, localPort: Int,
        reorderMs: Int, dataHoles: Int,
    ): String?

    @JvmStatic private external fun nativeStop()
    @JvmStatic private external fun nativeStatus(): String
    @JvmStatic private external fun nativeLocalPort(): Int
    @JvmStatic private external fun nativeLiveHoles(): Int

    /** Запускает клиента. Возвращает null при успехе, иначе текст ошибки. */
    fun start(config: Config): String? = nativeStart(
        config.stun, config.mqtt, config.caPem, config.myId, config.peerId, config.localPort, config.reorderMs, config.dataHoles,
    )

    fun stop() = nativeStop()

    /** Строка состояния: сколько дыр живо, счётчики пакетов. */
    fun status(): String = nativeStatus()

    /** Порт моста на 127.0.0.1 (0, если клиент не запущен). */
    fun localPort(): Int = nativeLocalPort()

    /** Сколько дыр к роутеру живо сейчас. VPN включаем, только когда их хотя бы одна. */
    fun liveHoles(): Int = nativeLiveHoles()

    data class Config(
        val stun: String,
        val mqtt: String,
        /** PEM с CA-сертификатом MQTT-брокера. */
        val caPem: String,
        /** GUID телефона. */
        val myId: String,
        /** GUID роутера (набор дыр «к этому телефону»). */
        val peerId: String,
        /** 0 — любой свободный порт. */
        val localPort: Int,
        /** Сколько мс ждать недостающий пакет WireGuard при восстановлении порядка (0 — не восстанавливать). */
        val reorderMs: Int,
        /** Через сколько дыр слать данные (0 — через все живые). */
        val dataHoles: Int,
    )
}
