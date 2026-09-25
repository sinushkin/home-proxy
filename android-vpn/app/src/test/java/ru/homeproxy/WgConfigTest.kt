package ru.homeproxy

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

class WgConfigTest {
    private val base = """
        [Interface]
        PrivateKey = AAAA
        Address = 10.9.0.2/32
        DNS = 10.9.0.1

        [Peer]
        PublicKey = BBBB
        AllowedIPs = 0.0.0.0/0
        Endpoint = vpn.example.org:51820
        PersistentKeepalive = 25
    """.trimIndent()

    private fun lines(text: String) = text.lines().map { it.trim() }

    @Test
    fun endpointPointsToTheLocalBridge() {
        val out = lines(WgConfig.prepare(base, 51821, "ru.homeproxy"))
        assertTrue("Endpoint = 127.0.0.1:51821" in out)
        assertFalse(out.any { it.contains("vpn.example.org") })
    }

    @Test
    fun missingEndpointIsAdded() {
        val text = base.lines().filterNot { it.startsWith("Endpoint") }.joinToString("\n")
        assertTrue("Endpoint = 127.0.0.1:60000" in lines(WgConfig.prepare(text, 60000, "ru.homeproxy")))
    }

    @Test
    fun ownAppIsExcludedFromTheTunnelAndMergedWithExisting() {
        val text = base.replace("DNS = 10.9.0.1", "DNS = 10.9.0.1\nExcludedApplications = com.a, com.b\nIncludedApplications = com.c")
        val out = lines(WgConfig.prepare(text, 1, "ru.homeproxy"))
        assertTrue("ExcludedApplications = com.a, com.b, ru.homeproxy" in out)
        assertFalse(out.any { it.startsWith("IncludedApplications") })
        // повторная подготовка не дублирует приложение
        val again = lines(WgConfig.prepare(WgConfig.prepare(text, 1, "ru.homeproxy"), 1, "ru.homeproxy"))
        assertEquals(1, again.count { it.startsWith("ExcludedApplications") })
        assertTrue("ExcludedApplications = com.a, com.b, ru.homeproxy" in again)
    }

    @Test
    fun mtuDefaultsAndIsCapped() {
        assertTrue("MTU = ${WgConfig.MTU}" in lines(WgConfig.prepare(base, 1, "x")))
        val big = base.replace("DNS = 10.9.0.1", "DNS = 10.9.0.1\nMTU = 1420")
        assertTrue("MTU = ${WgConfig.MAX_MTU}" in lines(WgConfig.prepare(big, 1, "x")))
        val small = base.replace("DNS = 10.9.0.1", "DNS = 10.9.0.1\nMTU = 1280")
        assertTrue("MTU = 1280" in lines(WgConfig.prepare(small, 1, "x")))
    }

    @Test
    fun everyPeerGetsTheBridgeEndpoint() {
        val two = base + "\n\n[Peer]\nPublicKey = CCCC\nAllowedIPs = 10.9.0.5/32\n"
        val out = WgConfig.prepare(two, 4242, "x")
        assertEquals(2, out.lines().count { it.trim() == "Endpoint = 127.0.0.1:4242" })
    }

    @Test
    fun brokenConfigsAreRejectedWithAReason() {
        val noPeer = "[Interface]\nPrivateKey = AAAA\n"
        val noKey = "[Interface]\nAddress = 10.0.0.2/32\n\n[Peer]\nPublicKey = B\n"
        val noInterface = "[Peer]\nPublicKey = B\n"
        for ((text, reason) in listOf(noPeer to "[Peer]", noKey to "PrivateKey", noInterface to "[Interface]")) {
            try {
                WgConfig.prepare(text, 1, "x")
                fail("ожидали ошибку: $reason")
            } catch (e: IllegalArgumentException) {
                assertTrue(e.message, e.message!!.contains(reason))
            }
        }
    }
}
