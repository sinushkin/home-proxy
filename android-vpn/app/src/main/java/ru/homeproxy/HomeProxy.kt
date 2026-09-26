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
    @JvmStatic private external fun nativeAddress(): String
    @JvmStatic private external fun nativeDns(): String
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
     * Адрес в туннеле, выданный сервером (домашним ПК или VPS за роутером): пара «адрес, префикс»
     * или null, пока не выдан. VPN поднимается с ним.
     */
    fun address(): Pair<String, Int>? {
        val text = nativeAddress()
        val slash = text.indexOf('/')
        if (slash <= 0) return null
        return Pair(text.substring(0, slash), text.substring(slash + 1).toIntOrNull() ?: return null)
    }

    /** DNS от сервера по порядку (его резолверы, затем 8.8.8.8 и 1.1.1.1); пусто, пока адреса нет. */
    fun dns(): List<String> = nativeDns().split(',').map { it.trim() }.filter { it.isNotEmpty() }

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
