package ru.homeproxy

import android.content.Context
import java.util.UUID

/** Настройки клиента в SharedPreferences (их читает и сервис, и экран). */
class Settings(context: Context) {
    private val prefs = context.getSharedPreferences("homeproxy", Context.MODE_PRIVATE)
    private val appContext = context.applicationContext

    var stun: String
        get() = prefs.getString("stun", "") ?: ""
        set(value) = prefs.edit().putString("stun", value).apply()

    var mqtt: String
        get() = prefs.getString("mqtt", "") ?: ""
        set(value) = prefs.edit().putString("mqtt", value).apply()

    var peerId: String
        get() = prefs.getString("peerId", "") ?: ""
        set(value) = prefs.edit().putString("peerId", value).apply()

    /**
     * Адрес телефона в туннеле (`/32`). За роутером — `10.80.1.<n>`, где `n` — номер телефона
     * в настройках роутера (`PHONE_<n>_*`): по этому адресу VPS отвечает именно ему.
     */
    var tunAddr: String
        get() = prefs.getString("tunAddr", DEFAULT_TUN_ADDR) ?: DEFAULT_TUN_ADDR
        set(value) = prefs.edit().putString("tunAddr", value).apply()

    /** DNS-сервер внутри VPN (запросы идут по туннелю). */
    var dns: String
        get() = prefs.getString("dns", DEFAULT_DNS) ?: DEFAULT_DNS
        set(value) = prefs.edit().putString("dns", value).apply()

    /** GUID телефона: создаётся при первом запуске и дальше не меняется. */
    var myId: String
        get() = prefs.getString("myId", null) ?: UUID.randomUUID().toString().also {
            prefs.edit().putString("myId", it).apply()
        }
        set(value) = prefs.edit().putString("myId", value).apply()

    /** CA брокера лежит в assets (build-native.sh копирует cert/out/ca.crt). */
    private fun caPem(): String = appContext.assets.open("ca.crt").bufferedReader().use { it.readText() }

    /**
     * Принимает настройки из intent (для проверки из adb):
     * `am start -n ru.homeproxy/.MainActivity --es stun ip:порт --es mqtt ip:порт
     * --es peerId GUID [--es myId GUID] [--es tunAddr 10.80.1.1] [--es dns 1.1.1.1]
     * [--ei reorderMs 8] [--ei dataHoles 1] --ez autostart true [--ez vpn true]`.
     */
    fun applyExtras(intent: android.content.Intent) {
        intent.getStringExtra("stun")?.let { stun = it }
        intent.getStringExtra("mqtt")?.let { mqtt = it }
        intent.getStringExtra("peerId")?.let { peerId = it }
        intent.getStringExtra("myId")?.let { myId = it }
        intent.getStringExtra("tunAddr")?.let { tunAddr = it }
        intent.getStringExtra("dns")?.let { dns = it }
        if (intent.hasExtra("reorderMs")) reorderMs = intent.getIntExtra("reorderMs", DEFAULT_REORDER_MS)
        if (intent.hasExtra("dataHoles")) dataHoles = intent.getIntExtra("dataHoles", 0)
    }

    var reorderMs: Int
        get() = prefs.getInt("reorderMs", DEFAULT_REORDER_MS)
        set(value) = prefs.edit().putInt("reorderMs", value).apply()

    var dataHoles: Int
        get() = prefs.getInt("dataHoles", 0)
        set(value) = prefs.edit().putInt("dataHoles", value).apply()

    fun toConfig() = HomeProxy.Config(
        stun = stun, mqtt = mqtt, caPem = caPem(), myId = myId, peerId = peerId,
        reorderMs = reorderMs, dataHoles = dataHoles,
    )

    companion object {
        const val DEFAULT_TUN_ADDR = "10.80.1.1"
        const val DEFAULT_DNS = "1.1.1.1"
        const val DEFAULT_REORDER_MS = 8
    }
}
