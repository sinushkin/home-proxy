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

    /** Конфиг WireGuard (формат wg-quick) целиком, как его выдаёт сервер. */
    var wgConfig: String
        get() = prefs.getString("wgConfig", null) ?: bundledWgConfig()
        set(value) = prefs.edit().putString("wgConfig", value).apply()

    /** Конфиг, вложенный в APK при сборке (build-native.sh копирует wireguard/out/client.conf). */
    private fun bundledWgConfig(): String = try {
        appContext.assets.open("wg.conf").bufferedReader().use { it.readText() }
    } catch (_: java.io.IOException) {
        ""
    }

    var localPort: Int
        get() = prefs.getInt("localPort", DEFAULT_LOCAL_PORT)
        set(value) = prefs.edit().putInt("localPort", value).apply()

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
     * --es peerId GUID [--es myId GUID] --ei localPort 51821 [--es wgConfigB64 …]
     * --ez autostart true [--ez vpn true]`.
     */
    fun applyExtras(intent: android.content.Intent) {
        intent.getStringExtra("stun")?.let { stun = it }
        intent.getStringExtra("mqtt")?.let { mqtt = it }
        intent.getStringExtra("peerId")?.let { peerId = it }
        intent.getStringExtra("myId")?.let { myId = it }
        // Конфиг WireGuard base64 (в одну строку, чтобы передать из adb).
        intent.getStringExtra("wgConfigB64")?.let {
            wgConfig = String(android.util.Base64.decode(it, android.util.Base64.DEFAULT))
        }
        if (intent.hasExtra("localPort")) localPort = intent.getIntExtra("localPort", DEFAULT_LOCAL_PORT)
    }

    fun toConfig() = HomeProxy.Config(
        stun = stun, mqtt = mqtt, caPem = caPem(), myId = myId, peerId = peerId, localPort = localPort,
    )

    companion object {
        const val DEFAULT_LOCAL_PORT = 51821
    }
}
