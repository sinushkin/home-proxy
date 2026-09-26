package ru.homeproxy

/**
 * Обёртка над libhomeproxy.so (Rust, hp-backend/android-lib). Имена native-методов
 * связаны с именем пакета: Java_ru_homeproxy_HomeProxy_*.
 *
 * Клиент держит 10 UDP-дыр к роутеру (или ПК). VPN — свой: дескриптор TUN от
 * [HpVpnService] отдаётся клиенту, IP-пакеты идут по дырам как есть, без WireGuard.
 */
object HomeProxy {
    init {
        System.loadLibrary("homeproxy")
    }

    @JvmStatic private external fun nativeStart(
        stun: String, mqtt: String, caPem: String, myId: String, peerId: String,
        reorderMs: Int, dataHoles: Int,
    ): String?

    @JvmStatic private external fun nativeStop()
    @JvmStatic private external fun nativeStatus(): String
    @JvmStatic private external fun nativeAttachTun(fd: Int): String?
    @JvmStatic private external fun nativeDetachTun()
    @JvmStatic private external fun nativeLiveHoles(): Int

    /** Запускает клиента. Возвращает null при успехе, иначе текст ошибки. */
    fun start(config: Config): String? = nativeStart(
        config.stun, config.mqtt, config.caPem, config.myId, config.peerId, config.reorderMs, config.dataHoles,
    )

    fun stop() = nativeStop()

    /** Строка состояния: сколько дыр живо, счётчики пакетов. */
    fun status(): String = nativeStatus()

    /**
     * Отдаёт клиенту дескриптор TUN (`ParcelFileDescriptor.detachFd()`: владение переходит
     * к клиенту). Возвращает null при успехе, иначе текст ошибки.
     */
    fun attachTun(fd: Int): String? = nativeAttachTun(fd)

    /** Отключает TUN (дескриптор закрывается), дыры остаются. */
    fun detachTun() = nativeDetachTun()

    /** Сколько дыр к роутеру живо сейчас. VPN включаем, только когда их хотя бы одна. */
    fun liveHoles(): Int = nativeLiveHoles()

    data class Config(
        val stun: String,
        val mqtt: String,
        /** PEM с CA-сертификатом MQTT-брокера. */
        val caPem: String,
        /** GUID телефона. */
        val myId: String,
        /** GUID роутера или ПК (набор дыр «к этому телефону»). */
        val peerId: String,
        /** Сколько мс ждать недостающий TCP-пакет при восстановлении порядка (0 — не восстанавливать). */
        val reorderMs: Int,
        /** Через сколько дыр слать данные (0 — через все живые). */
        val dataHoles: Int,
    )
}
