package ru.homeproxy

/**
 * Готовит конфиг WireGuard (формат wg-quick) к работе через наш мост:
 *  - у каждого пира `Endpoint` подменяется на `127.0.0.1:<порт моста>`;
 *  - приложение исключается из туннеля (`ExcludedApplications`): иначе UDP-дыры и
 *    MQTT-соединение пошли бы внутрь самого VPN;
 *  - `IncludedApplications` убирается (с исключением он не совместим);
 *  - MTU по умолчанию [MTU], больше [MAX_MTU] не допускается: пакет WireGuard на
 *    32 байта длиннее вложенного, а в дыру влезает не больше 1400 байт.
 */
object WgConfig {
    const val MTU = 1360
    const val MAX_MTU = 1368

    private class Section(val name: String, val lines: MutableList<String>)

    fun prepare(text: String, localPort: Int, excludedApp: String): String {
        val sections = split(text)
        val iface = sections.firstOrNull { it.name == "interface" }
            ?: throw IllegalArgumentException("нет секции [Interface]")
        val peers = sections.filter { it.name == "peer" }
        if (peers.isEmpty()) throw IllegalArgumentException("нет секции [Peer]")
        if (iface.lines.none { key(it) == "privatekey" }) {
            throw IllegalArgumentException("в [Interface] нет PrivateKey")
        }

        prepareInterface(iface, excludedApp)
        peers.forEach { setValue(it, "Endpoint", "127.0.0.1:$localPort") }
        return sections.joinToString("\n") { section ->
            (listOf("[${section.name.replaceFirstChar { it.uppercase() }}]") + section.lines).joinToString("\n")
        } + "\n"
    }

    private fun prepareInterface(iface: Section, excludedApp: String) {
        iface.lines.removeAll { key(it) == "includedapplications" }

        val excluded = valueOf(iface, "excludedapplications")
            ?.split(',')?.map { it.trim() }?.filter { it.isNotEmpty() }.orEmpty()
        setValue(iface, "ExcludedApplications", (excluded + excludedApp).distinct().joinToString(", "))

        val mtu = valueOf(iface, "mtu")?.toIntOrNull()
        setValue(iface, "MTU", (mtu?.coerceAtMost(MAX_MTU) ?: MTU).toString())
    }

    private fun split(text: String): List<Section> {
        val sections = mutableListOf<Section>()
        for (raw in text.lines()) {
            val line = raw.trimEnd()
            if (line.trim().startsWith("[") && line.contains("]")) {
                sections += Section(line.trim().removePrefix("[").substringBefore("]").trim().lowercase(), mutableListOf())
            } else {
                sections.lastOrNull()?.lines?.add(line)
            }
        }
        // Пустые строки в хвосте секций ни к чему.
        sections.forEach { section -> while (section.lines.lastOrNull()?.isBlank() == true) section.lines.removeAt(section.lines.lastIndex) }
        return sections
    }

    /** Имя ключа строки `Key = value` в нижнем регистре, иначе null (комментарии, пустые). */
    private fun key(line: String): String? {
        val body = line.substringBefore('#').trim()
        if (!body.contains('=')) return null
        return body.substringBefore('=').trim().lowercase()
    }

    private fun valueOf(section: Section, key: String): String? =
        section.lines.firstOrNull { key(it) == key }?.substringBefore('#')?.substringAfter('=')?.trim()

    private fun setValue(section: Section, name: String, value: String) {
        val index = section.lines.indexOfFirst { key(it) == name.lowercase() }
        val line = "$name = $value"
        if (index >= 0) section.lines[index] = line else section.lines.add(line)
    }
}
