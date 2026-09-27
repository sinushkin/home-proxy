package ru.homeproxy

/** Разобранный пакет сопряжения (protobuf `PairingBundle`, `control/proto/control.proto`). */
data class PairingBundle(
    val pcGuid: String,
    val phoneGuid: String,
    val stun: String,
    val mqtt: String,
    val caPem: String,
)

/**
 * Ссылка сопряжения `homeproxy://pair?d=<base64url(PairingBundle)>` из QR трея (`control/README.md`):
 * служба сама завела пару GUID и набор дыр, здесь их только разбираем. Протобуф — свой минимальный
 * декодер (сообщение плоское, полей мало), сторонней библиотеки не тянем.
 */
object Pairing {
    private const val PREFIX = "homeproxy://pair?d="
    private const val ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"

    fun parse(uri: String): PairingBundle? {
        val data = uri.trim().takeIf { it.startsWith(PREFIX) }?.substring(PREFIX.length) ?: return null
        val bytes = base64UrlDecode(data) ?: return null
        var pcGuid = ""
        var phoneGuid = ""
        var stun = ""
        var mqtt = ""
        var caPem = ""
        var i = 0
        while (i < bytes.size) {
            val (tag, afterTag) = readVarint(bytes, i) ?: return null
            i = afterTag
            val field = (tag ushr 3).toInt()
            when ((tag and 0x7).toInt()) {
                0 -> { // varint (version, expires_unix) — не нужны для подключения
                    val (_, next) = readVarint(bytes, i) ?: return null
                    i = next
                }
                2 -> { // length-delimited: строка или байты
                    val (len, afterLen) = readVarint(bytes, i) ?: return null
                    val start = afterLen
                    val end = start + len.toInt()
                    if (len < 0 || len > Int.MAX_VALUE || end > bytes.size) return null
                    val text = String(bytes, start, len.toInt(), Charsets.UTF_8)
                    when (field) {
                        2 -> pcGuid = text
                        3 -> phoneGuid = text
                        4 -> stun = text
                        5 -> mqtt = text
                        6 -> caPem = text
                    }
                    i = end
                }
                else -> return null // fixed32/fixed64 в этом сообщении не встречаются
            }
        }
        if (pcGuid.isEmpty() || phoneGuid.isEmpty() || stun.isEmpty() || mqtt.isEmpty() || caPem.isEmpty()) return null
        return PairingBundle(pcGuid, phoneGuid, stun, mqtt, caPem)
    }

    /** protobuf varint (little-endian base-128): значение и индекс сразу за ним. */
    private fun readVarint(bytes: ByteArray, start: Int): kotlin.Pair<Long, Int>? {
        var result = 0L
        var shift = 0
        var i = start
        while (true) {
            if (i >= bytes.size || shift > 63) return null
            val b = bytes[i].toInt() and 0xFF
            result = result or ((b.toLong() and 0x7F) shl shift)
            i++
            if (b and 0x80 == 0) break
            shift += 7
        }
        return kotlin.Pair(result, i)
    }

    /** Тот же алфавит и порядок бит, что у `base64url_encode` в `hp-control` (без `=`). */
    private fun base64UrlDecode(text: String): ByteArray? {
        val out = ArrayList<Byte>(text.length * 3 / 4 + 1)
        var acc = 0
        var bits = 0
        for (c in text) {
            val v = ALPHABET.indexOf(c)
            if (v < 0) return null
            acc = (acc shl 6) or v
            bits += 6
            if (bits >= 8) {
                bits -= 8
                out.add(((acc shr bits) and 0xFF).toByte())
            }
        }
        return out.toByteArray()
    }
}
