package ru.homeproxy

import android.Manifest
import android.app.Activity
import android.content.Intent
import android.content.pm.PackageManager
import android.net.VpnService
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.ViewGroup
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import kotlin.concurrent.thread

/**
 * Экран. Порядок такой:
 *  1. «Подключить» — поднимает клиента: дыры к роутеру и keep-alive по ним;
 *  2. «Включить VPN» — доступна, только когда хотя бы одна дыра уже живая.
 */
class MainActivity : Activity() {
    private lateinit var settings: Settings
    private lateinit var stun: EditText
    private lateinit var mqtt: EditText
    private lateinit var peerId: EditText
    private lateinit var tunAddr: EditText
    private lateinit var dns: EditText
    private lateinit var status: TextView
    private lateinit var connectButton: Button
    private lateinit var vpnOnButton: Button
    private lateinit var vpnOffButton: Button
    private val handler = Handler(Looper.getMainLooper())

    @Volatile private var vpnMessage: String? = null

    private val refresh = object : Runnable {
        override fun run() {
            render()
            handler.postDelayed(this, 1000)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        settings = Settings(this)
        settings.applyExtras(intent)

        val column = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(32, 32, 32, 32)
        }
        column.addView(label("Мой GUID"))
        column.addView(TextView(this).apply {
            text = settings.myId
            setTextIsSelectable(true)
        })
        stun = field(column, "STUN (ip:порт)", settings.stun)
        mqtt = field(column, "MQTT, TLS (ip:порт)", settings.mqtt)
        peerId = field(column, "GUID пира (ПК или роутера)", settings.peerId)
        tunAddr = field(column, "Адрес в туннеле (10.80.1.<номер телефона на роутере>)", settings.tunAddr)
        dns = field(column, "DNS внутри VPN", settings.dns)

        connectButton = button(column, "1. Подключить (дыры и keep-alive)") { connect() }
        vpnOnButton = button(column, "2. Включить VPN") { startVpn() }
        vpnOffButton = button(column, "Выключить VPN") { VpnController.stop(applicationContext) }
        button(column, "Остановить всё") { stopAll() }
        status = TextView(this).apply { setPadding(0, 24, 0, 0) }
        column.addView(status)

        setContentView(ScrollView(this).apply {
            addView(column, ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT)
        })

        if (Build.VERSION.SDK_INT >= 33 &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
        ) {
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 1)
        }
    }

    override fun onStart() {
        super.onStart()
        // Автозапуск для проверки из adb (см. Settings.applyExtras): клиент, а потом,
        // когда появятся живые дыры, и VPN.
        if (intent.getBooleanExtra("autostart", false)) {
            intent.removeExtra("autostart")
            connect()
            if (intent.getBooleanExtra("vpn", false)) {
                intent.removeExtra("vpn")
                thread(name = "homeproxy-autovpn") {
                    repeat(90) {
                        if (VpnController.canStart()) {
                            runOnUiThread { startVpn() }
                            return@thread
                        }
                        Thread.sleep(1000)
                    }
                    vpnMessage = "автозапуск VPN: дыры не поднялись за 90 секунд"
                }
            }
        }
    }

    override fun onResume() {
        super.onResume()
        handler.post(refresh)
    }

    override fun onPause() {
        handler.removeCallbacks(refresh)
        super.onPause()
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode == REQUEST_VPN) {
            if (resultCode == RESULT_OK) launchVpn() else vpnMessage = "согласие на VPN не получено"
        }
    }

    private fun render() {
        val error = ProxyService.lastError
        val holes = HomeProxy.liveHoles()
        status.text = buildString {
            append(if (error != null) "Ошибка: $error" else HomeProxy.status())
            append("\nVPN: ").append(if (VpnController.isUp) "включён" else "выключен")
            (vpnMessage ?: VpnController.lastMessage)?.let { append("\n").append(it) }
            if (holes < 1) append("\nVPN можно включить, когда появится хотя бы одна живая дыра")
        }
        vpnOnButton.isEnabled = holes >= 1 && !VpnController.isUp
        vpnOffButton.isEnabled = VpnController.isUp
    }

    private fun save() {
        settings.stun = stun.text.toString().trim()
        settings.mqtt = mqtt.text.toString().trim()
        settings.peerId = peerId.text.toString().trim()
        settings.tunAddr = tunAddr.text.toString().trim()
        settings.dns = dns.text.toString().trim()
    }

    private fun connect() {
        save()
        vpnMessage = null
        ProxyService.lastError = null
        ProxyService.start(this)
    }

    private fun startVpn() {
        save()
        if (!VpnController.canStart()) {
            vpnMessage = "дыр ещё нет: сначала «Подключить»"
            return
        }
        val consent = VpnService.prepare(this)
        if (consent != null) startActivityForResult(consent, REQUEST_VPN) else launchVpn()
    }

    private fun launchVpn() {
        vpnMessage = null
        VpnController.start(applicationContext)
    }

    private fun stopAll() {
        vpnMessage = null
        VpnController.stop(applicationContext)
        ProxyService.stop(this)
    }

    private fun label(text: String) = TextView(this).apply { this.text = text; setPadding(0, 24, 0, 0) }

    private fun button(parent: LinearLayout, text: String, onClick: () -> Unit) =
        Button(this).apply {
            this.text = text
            setOnClickListener { onClick() }
        }.also { parent.addView(it) }

    private fun field(parent: LinearLayout, hint: String, value: String): EditText {
        parent.addView(label(hint))
        return EditText(this).apply { setText(value); setSingleLine() }.also { parent.addView(it) }
    }

    companion object {
        private const val REQUEST_VPN = 100
    }
}
