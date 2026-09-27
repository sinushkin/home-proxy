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


    /** GUID телефона: создаётся при первом запуске и дальше не меняется. */
    var myId: String
        get() = prefs.getString("myId", null) ?: UUID.randomUUID().toString().also {
            prefs.edit().putString("myId", it).apply()
        }
        set(value) = prefs.edit().putString("myId", value).apply()

    /** CA из пакета сопряжения (QR), если сканировали; иначе — из assets (сборка копирует cert/out/ca.crt). */
    var caPemOverride: String?
        get() = prefs.getString("caPem", null)
        set(value) = prefs.edit().putString("caPem", value).apply()

    private fun caPem(): String =
        caPemOverride ?: appContext.assets.open("ca.crt").bufferedReader().use { it.readText() }

    /** Пакет сопряжения (`Pairing.parse`) заменяет GUID'ы, рандеву и CA и сохраняет их. */
    fun applyPairing(bundle: PairingBundle) {
        myId = bundle.phoneGuid
        peerId = bundle.pcGuid
        stun = bundle.stun
        mqtt = bundle.mqtt
        caPemOverride = bundle.caPem
    }

    /**
     * Принимает настройки из intent (для проверки из adb):
     * `am start -n ru.homeproxy/.MainActivity --es stun ip:порт --es mqtt ip:порт
     * --es peerId GUID [--es myId GUID]
     * [--ei reorderMs 30] [--ei dataHoles 1] --ez autostart true [--ez vpn true]`.
     */
    fun applyExtras(intent: android.content.Intent) {
        intent.getStringExtra("stun")?.let { stun = it }
        intent.getStringExtra("mqtt")?.let { mqtt = it }
        intent.getStringExtra("peerId")?.let { peerId = it }
        intent.getStringExtra("myId")?.let { myId = it }
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
        // Предел буфера порядка (connection::reorder::MAX_WAIT) — не адаптивное начальное
        // значение, а сразу максимум: меньше шанс отдать пакет не по порядку на неровной сети.
        const val DEFAULT_REORDER_MS = 30
    }
}
