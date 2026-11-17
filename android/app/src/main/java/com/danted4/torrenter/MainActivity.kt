package com.danted4.torrenter

import android.Manifest
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Environment
import android.os.PowerManager
import android.provider.Settings
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.HorizontalDivider
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.TextButton
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.torrenter.Engine
import uniffi.torrenter.JobView
import uniffi.torrenter.Kind
import uniffi.torrenter.banner
import uniffi.torrenter.debridServices

class MainActivity : ComponentActivity() {
    private val notify =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { }

    /** A .torrent handed over by "Open with" (or a browser download), queued
     *  once the engine is open. */
    val openedTorrent = mutableStateOf<Uri?>(null)

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        torrentUri(intent)?.let { openedTorrent.value = it }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (savedInstanceState == null) {
            torrentUri(intent)?.let { openedTorrent.value = it }
        }
        if (Build.VERSION.SDK_INT >= 33) {
            notify.launch(Manifest.permission.POST_NOTIFICATIONS)
        }
        val prefs = getSharedPreferences(Prefs.NAME, Context.MODE_PRIVATE)
        setContent {
            var themeId by remember { mutableStateOf(Prefs.theme(prefs)) }
            val colors = palette(themeId)
            MaterialTheme(
                colorScheme =
                    darkColorScheme(
                        primary = colors.fg,
                        onPrimary = colors.bg,
                        background = colors.bg,
                        surface = colors.row,
                        onBackground = colors.fg,
                        onSurface = colors.fg,
                        error = colors.fail,
                    ),
            ) {
                Surface(modifier = Modifier.fillMaxSize(), color = colors.bg) {
                    AppScreen(
                        activity = this,
                        palette = colors,
                        themeId = themeId,
                        onTheme = { id ->
                            themeId = id
                            Prefs.saveTheme(prefs, id)
                        },
                    )
                }
            }
        }
    }
}

@androidx.compose.runtime.Composable
private fun AppScreen(
    activity: MainActivity,
    palette: Palette,
    themeId: ThemeId,
    onTheme: (ThemeId) -> Unit,
) {
    val app = activity.application as TorrenterApp
    val prefs = activity.getSharedPreferences(Prefs.NAME, Context.MODE_PRIVATE)
    var allowed by remember { mutableStateOf(Environment.isExternalStorageManager()) }
    var notice by remember { mutableStateOf<String?>(null) }
    var batteryWarned by remember { mutableStateOf(false) }
    var input by remember { mutableStateOf("") }
    var magnet by remember { mutableStateOf(true) }
    var seed by remember { mutableStateOf(Prefs.seed(prefs)) }
    var jobs by remember { mutableStateOf(listOf<JobView>()) }
    var engine by remember { mutableStateOf<Engine?>(null) }
    var settingsOpen by remember { mutableStateOf(false) }
    var debridRoute by remember { mutableStateOf(Prefs.debridRoute(prefs)) }
    var connections by remember { mutableStateOf(Prefs.connections(prefs)) }
    var removeAfter by remember { mutableStateOf(Prefs.removeAfter(prefs)) }
    var accounts by remember { mutableStateOf(Accounts.load(activity)) }
    var settingsTab by remember { mutableStateOf(SettingsTab.APPEARANCE) }
    val notes = remember { mutableStateMapOf<String, String>() }
    var clearAsk by remember { mutableStateOf<JobView?>(null) }
    // "as torrent" on a debrid job torrenter added: ask before it leaves the service.
    var rerouteAsk by remember { mutableStateOf<JobView?>(null) }
    val scope = rememberCoroutineScope()
    val buttons =
        ButtonDefaults.buttonColors(
            containerColor = palette.rowAlt,
            contentColor = palette.fg,
            disabledContainerColor = palette.row,
            disabledContentColor = palette.dim,
        )

    fun refresh() {
        val live = engine ?: return
        app.takeNotices().lastOrNull()?.let { notice = it }
        jobs = live.jobs()
        if (live.anyActive()) {
            DownloadService.startFromUi(activity)
        }
    }

    // A .torrent from the picker or "Open with": read (capped) off the main
    // thread, then queued at once through the current route, like START.
    fun queueTorrent(uri: Uri) {
        val live = engine ?: return
        scope.launch {
            try {
                val bytes = withContext(Dispatchers.IO) { readTorrent(activity, uri) }
                live.queueTorrentFile(bytes, debridRoute)
                notice = null
                refresh()
            } catch (error: Exception) {
                notice = error.message
            }
        }
    }
    val pickTorrent =
        rememberLauncherForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
            uri?.let { queueTorrent(it) }
        }

    // Stop waiting for the service and download the same torrent from peers.
    fun asTorrent(
        job: JobView,
        removeRemote: Boolean,
    ) {
        try {
            engine?.restartRoute(job.id, null, seed, job.dest, removeRemote)
            refresh()
        } catch (error: Exception) {
            notice = error.message
        }
    }

    fun setShare(value: Boolean) {
        seed = value
        engine?.setSeed(value)
        Prefs.saveSeed(prefs, value)
    }

    fun setRoute(id: String?) {
        debridRoute = id
        Prefs.saveDebridRoute(prefs, id)
    }

    fun account(id: String?): DebridAccount? = accounts.firstOrNull { it.id == id }

    /** "work (TorBox)", or just "TorBox" when the name is the provider's. */
    fun accountTitle(job: JobView): String {
        val account = account(job.account) ?: return serviceLabel(job.service)
        val provider = serviceLabel(account.service)
        return if (account.name == provider) provider else "${account.name} ($provider)"
    }

    fun testAccount(id: String) {
        val live = engine ?: return
        val service = account(id)?.service ?: return
        val key = SecureKeys.read(activity, id) ?: return
        notes[id] = "checking…"
        scope.launch {
            val note =
                withContext(Dispatchers.IO) {
                    try {
                        val result = live.debridAccount(service, key)
                        if (result.premium) {
                            "premium" + (result.expires?.let { " until $it" } ?: "")
                        } else {
                            "key works, but the account is not premium"
                        }
                    } catch (error: Exception) {
                        error.message ?: "check failed"
                    }
                }
            notes[id] = note
        }
    }

    /** Save a new account: the key encrypted by id, the name in prefs. */
    fun addAccount(
        service: String,
        name: String,
        key: String,
    ): Boolean {
        val typed = name.trim()
        if (key.isBlank() || (typed.isNotEmpty() && Accounts.nameTaken(typed, accounts))) return false
        val finalName = typed.ifEmpty { Accounts.defaultName(serviceLabel(service), accounts) }
        val id = Accounts.newId(service, accounts)
        if (!SecureKeys.save(activity, id, key.trim())) {
            notice = "could not save the key"
            return false
        }
        engine?.setDebridAccount(id, service, key.trim())
        val added = DebridAccount(id, finalName, service)
        accounts = accounts + added
        Accounts.add(activity, added)
        testAccount(id)
        return true
    }

    fun removeAccount(id: String) {
        val gone = account(id) ?: return
        SecureKeys.delete(activity, id)
        engine?.setDebridAccount(id, gone.service, null)
        accounts = accounts.filterNot { it.id == id }
        Accounts.remove(activity, id)
        notes.remove(id)
        if (debridRoute == id) {
            setRoute(null)
        }
    }

    BackHandler(enabled = settingsOpen) { settingsOpen = false }

    LaunchedEffect(allowed) {
        if (!allowed) {
            return@LaunchedEffect
        }
        try {
            val opened = app.openEngine(seed)
            engine = opened
            seed = opened.seed()
            if (debridRoute != null && account(debridRoute) == null) {
                setRoute(null)
            }
            if (!batteryWarned && !isIgnoringBattery(activity)) {
                notice =
                    "the OS may stop seeding in the background unless battery optimization is ignored"
                requestBatteryExemption(activity)
                batteryWarned = true
            }
            refresh()
        } catch (error: Exception) {
            notice = error.message
        }
    }

    val opened = activity.openedTorrent.value
    LaunchedEffect(engine, opened) {
        if (engine != null && opened != null) {
            activity.openedTorrent.value = null
            queueTorrent(opened)
        }
    }

    LaunchedEffect(engine) {
        if (engine == null) {
            return@LaunchedEffect
        }
        while (true) {
            delay(200)
            refresh()
        }
    }

    Column(
        modifier = Modifier.fillMaxSize().padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Row(
            modifier = Modifier.fillMaxWidth(),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            // The title doubles as "home": back to the main screen.
            Row(
                modifier = Modifier.weight(1f).clickable { settingsOpen = false },
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                Image(
                    painter = painterResource(R.drawable.ic_brand),
                    contentDescription = "torrenter",
                    modifier = Modifier.size(28.dp),
                )
                Text(
                    banner(),
                    color = palette.fg,
                    fontFamily = FontFamily.Monospace,
                    style = MaterialTheme.typography.titleMedium,
                )
            }
            // A back arrow while settings fill the screen.
            CogGlyph(
                color = if (settingsOpen) palette.fg else palette.dim,
                open = settingsOpen,
                onClick = { settingsOpen = !settingsOpen },
            )
        }
        if (!allowed) {
            Text("All files access is required so downloads go to Download/torrenter.", color = palette.dim)
            Button(onClick = { requestAllFiles(activity) }, colors = buttons, shape = RoundedCornerShape(2.dp)) {
                Text("grant all files access", fontFamily = FontFamily.Monospace)
            }
            Button(
                onClick = { allowed = Environment.isExternalStorageManager() },
                colors = buttons,
                shape = RoundedCornerShape(2.dp),
            ) { Text("I granted it", fontFamily = FontFamily.Monospace) }
            return@Column
        }
        notice?.let { Text(it, color = palette.fail, fontFamily = FontFamily.Monospace, fontSize = 13.sp) }
        if (settingsOpen) {
            SettingsScreen(
                palette = palette,
                themeId = themeId,
                onTheme = onTheme,
                buttons = buttons,
                tab = settingsTab,
                onTab = { settingsTab = it },
                accounts = accounts,
                notes = notes,
                connections = connections,
                onConnections = {
                    connections = it
                    Prefs.saveConnections(prefs, it)
                    engine?.setConnections(Prefs.connectionsArg(it))
                },
                removeAfter = removeAfter,
                onRemoveAfter = {
                    removeAfter = it
                    Prefs.saveRemoveAfter(prefs, it)
                    engine?.setDebridRemoveAfter(it)
                },
                onAddAccount = { service, name, key -> addAccount(service, name, key) },
                onTestAccount = { testAccount(it) },
                onRemoveAccount = { removeAccount(it) },
            )
            return@Column
        }
        ModeSwitch(magnet = magnet, palette = palette, onToggle = { magnet = !magnet })
        OutlinedTextField(
            value = input,
            onValueChange = { input = it },
            modifier = Modifier.fillMaxWidth(),
            placeholder = {
                Text(
                    if (magnet) "magnet:?xt=urn:btih:…" else "https://example.com/file.iso",
                    fontFamily = FontFamily.Monospace,
                )
            },
            singleLine = true,
            trailingIcon =
                if (magnet) {
                    {
                        UploadGlyph(color = palette.dim) {
                            pickTorrent.launch(arrayOf("application/x-bittorrent", "application/octet-stream"))
                        }
                    }
                } else {
                    null
                },
            textStyle =
                androidx.compose.ui.text.TextStyle(
                    color = palette.fg,
                    fontFamily = FontFamily.Monospace,
                    fontSize = 14.sp,
                ),
            colors =
                OutlinedTextFieldDefaults.colors(
                    focusedBorderColor = palette.fg,
                    unfocusedBorderColor = palette.dim,
                    focusedTextColor = palette.fg,
                    unfocusedTextColor = palette.fg,
                    cursorColor = palette.fg,
                    focusedPlaceholderColor = palette.dim,
                    unfocusedPlaceholderColor = palette.dim,
                    focusedContainerColor = palette.row,
                    unfocusedContainerColor = palette.row,
                ),
            shape = RoundedCornerShape(2.dp),
        )
        Row(
            modifier = Modifier.fillMaxWidth(),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            SeedToggle(
                seed = seed,
                enabled = magnet && debridRoute == null,
                palette = palette,
                onChange = { setShare(it) },
            )
            if (magnet) {
                RouteMenu(
                    current = account(debridRoute),
                    accounts = accounts,
                    palette = palette,
                    modifier = Modifier.weight(1f),
                    onPick = { setRoute(it) },
                    onManage = {
                        settingsTab = SettingsTab.ACCOUNTS
                        settingsOpen = true
                    },
                )
            }
            Button(
                onClick = {
                    val live = engine ?: return@Button
                    try {
                        val route = debridRoute
                        if (magnet && route != null) {
                            live.queueDebrid(input.trim(), route)
                        } else {
                            live.queue(if (magnet) Kind.MAGNET else Kind.HTTP, input.trim())
                        }
                        input = ""
                        notice = null
                        refresh()
                    } catch (error: Exception) {
                        notice = error.message
                    }
                },
                colors = buttons,
                shape = RoundedCornerShape(2.dp),
                // One line always: the route select takes the spare width in
                // MAGNET mode, START does in HTTP mode.
                modifier = if (magnet) Modifier else Modifier.weight(1f),
            ) { Text("START", fontFamily = FontFamily.Monospace, maxLines = 1, softWrap = false) }
        }
        Text("Download/torrenter", color = palette.dim, fontFamily = FontFamily.Monospace, fontSize = 12.sp)
        if (jobs.isEmpty()) {
            Text("no jobs yet — paste a magnet or HTTP URL", color = palette.dim, fontFamily = FontFamily.Monospace)
        } else {
            LazyColumn(verticalArrangement = Arrangement.spacedBy(10.dp), modifier = Modifier.weight(1f)) {
                items(jobs, key = { it.id.toString() }) { job ->
                    JobCard(
                        job = job,
                        names = accounts.associate { it.id to it.name },
                        palette = palette,
                        buttons = buttons,
                        onPause = {
                            engine?.togglePause(job.id)
                            refresh()
                        },
                        onStop = {
                            engine?.stop(job.id)
                            refresh()
                        },
                        onClear = {
                            if (engine?.clearAsksAboutRemote(job.id) == true) {
                                clearAsk = job
                            } else {
                                engine?.remove(job.id)
                                refresh()
                            }
                        },
                        onAsTorrent = {
                            if (engine?.rerouteAsksAboutRemote(job.id, null) == true) {
                                rerouteAsk = job
                            } else {
                                asTorrent(job, false)
                            }
                        },
                    )
                }
            }
        }
        // Clear or "as torrent" on a debrid job torrenter added: same question.
        // Dismissing it does neither.
        (clearAsk ?: rerouteAsk)?.let { job ->
            val label = accountTitle(job)
            val rerouting = clearAsk == null
            val answer: (Boolean) -> Unit = { removeRemote ->
                clearAsk = null
                rerouteAsk = null
                if (rerouting) {
                    asTorrent(job, removeRemote)
                } else {
                    if (removeRemote) engine?.removeWithRemote(job.id) else engine?.remove(job.id)
                    refresh()
                }
            }
            AlertDialog(
                onDismissRequest = {
                    clearAsk = null
                    rerouteAsk = null
                },
                title = { Text("Also remove it from $label?", fontFamily = FontFamily.Monospace) },
                confirmButton = {
                    TextButton(onClick = { answer(true) }) {
                        Text("remove from $label", color = palette.fail, fontFamily = FontFamily.Monospace)
                    }
                },
                dismissButton = {
                    TextButton(onClick = { answer(false) }) {
                        Text("keep it on $label", color = palette.fg, fontFamily = FontFamily.Monospace)
                    }
                },
                containerColor = palette.row,
            )
        }
    }
}

@androidx.compose.runtime.Composable
private fun JobCard(
    job: JobView,
    names: Map<String, String>,
    palette: Palette,
    buttons: androidx.compose.material3.ButtonColors,
    onPause: () -> Unit,
    onStop: () -> Unit,
    onClear: () -> Unit,
    onAsTorrent: () -> Unit,
) {
    val fraction = jobBarFraction(job)
    val accent = if (job.active) mix(palette.fg, palette.dim, 0.65f) else palette.fg
    val lineColor = if (job.error != null) palette.fail else palette.dim
    Column(
        modifier =
            Modifier
                .fillMaxWidth()
                .background(palette.row, RoundedCornerShape(4.dp))
                .padding(10.dp),
        verticalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        if (job.kind != Kind.MAGNET) {
            Text(jobBadge(job, names), color = palette.dim, fontFamily = FontFamily.Monospace, fontSize = 11.sp)
        }
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            if (job.kind == Kind.MAGNET) {
                SeedGlyph(
                    share = job.seed,
                    selected = true,
                    faded = false,
                    palette = palette,
                )
            }
            Text(
                job.title.ifEmpty { job.source },
                color = accent,
                fontFamily = FontFamily.Monospace,
                fontSize = 13.sp,
                modifier = Modifier.weight(1f),
            )
        }
        if (fraction == null) {
            LinearProgressIndicator(
                modifier = Modifier.fillMaxWidth(),
                color = accent,
                trackColor = palette.rowAlt,
            )
        } else {
            LinearProgressIndicator(
                progress = { fraction },
                modifier = Modifier.fillMaxWidth(),
                color = accent,
                trackColor = palette.rowAlt,
            )
        }
        Text(jobStatsLine(job, names), color = lineColor, fontFamily = FontFamily.Monospace, fontSize = 12.sp)
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            if (job.active) {
                Button(onClick = onPause, colors = buttons, shape = RoundedCornerShape(2.dp)) {
                    Text(if (job.paused) "resume" else "pause", fontFamily = FontFamily.Monospace)
                }
                Button(onClick = onStop, colors = buttons, shape = RoundedCornerShape(2.dp)) {
                    Text("stop", fontFamily = FontFamily.Monospace)
                }
                if (jobIsFetching(job)) {
                    Button(onClick = onAsTorrent, colors = buttons, shape = RoundedCornerShape(2.dp)) {
                        Text("as torrent", fontFamily = FontFamily.Monospace)
                    }
                }
            } else {
                Button(onClick = onClear, colors = buttons, shape = RoundedCornerShape(2.dp)) {
                    Text("clear", fontFamily = FontFamily.Monospace)
                }
            }
        }
    }
}

/**
 * Fixed-width route dropdown: "default (none)" (peers) or an account by name,
 * shortened with "…". The list runs edge to edge (name left, provider right,
 * a dot on the current pick) and ends with a centred "manage accounts…".
 */
@androidx.compose.runtime.Composable
private fun RouteMenu(
    current: DebridAccount?,
    accounts: List<DebridAccount>,
    palette: Palette,
    modifier: Modifier = Modifier,
    onPick: (String?) -> Unit,
    onManage: () -> Unit,
) {
    var open by remember { mutableStateOf(false) }
    var boxWidth by remember { mutableStateOf(ROUTE_LIST_W) }
    val density = LocalDensity.current
    val on = current != null
    Box(modifier) {
        Box(
            modifier =
                Modifier
                    .fillMaxWidth()
                    .height(40.dp)
                    .onGloballyPositioned { boxWidth = with(density) { it.size.width.toDp() } }
                    .border(1.dp, if (on) palette.fg else palette.dim, RoundedCornerShape(2.dp))
                    .clickable { open = true },
            contentAlignment = Alignment.Center,
        ) {
            Text(
                current?.name ?: "default (none)",
                color = if (on) palette.fg else palette.dim,
                fontFamily = FontFamily.Monospace,
                fontSize = 13.sp,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.padding(horizontal = 20.dp),
            )
            Text(
                "▾",
                color = if (on) palette.fg else palette.dim,
                fontSize = 13.sp,
                modifier = Modifier.align(Alignment.CenterEnd).padding(end = 8.dp),
            )
        }
        // Never narrower than the box; wide enough for name + provider (the
        // desktop route list is wider than its box too).
        DropdownMenu(
            expanded = open,
            onDismissRequest = { open = false },
            modifier = Modifier.background(palette.row).width(maxOf(ROUTE_LIST_W, boxWidth)),
        ) {
            RouteRow("default (none)", "from peers", current == null, palette) {
                open = false
                onPick(null)
            }
            accounts.forEach { account ->
                RouteRow(account.name, serviceLabel(account.service), current?.id == account.id, palette) {
                    open = false
                    onPick(account.id)
                }
            }
            HorizontalDivider(color = palette.dim.copy(alpha = 0.4f))
            DropdownMenuItem(
                text = {
                    Text(
                        if (accounts.isEmpty()) "add a debrid account…" else "manage accounts…",
                        color = palette.dim,
                        fontFamily = FontFamily.Monospace,
                        fontSize = 13.sp,
                        textAlign = TextAlign.Center,
                        modifier = Modifier.fillMaxWidth(),
                    )
                },
                onClick = {
                    open = false
                    onManage()
                },
            )
        }
    }
}

/** The route list's narrowest width: room for a name and its provider. */
private val ROUTE_LIST_W = 240.dp

@androidx.compose.runtime.Composable
private fun RouteRow(
    name: String,
    detail: String,
    selected: Boolean,
    palette: Palette,
    onClick: () -> Unit,
) {
    DropdownMenuItem(
        text = {
            Row(modifier = Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                Box(Modifier.width(14.dp)) {
                    if (selected) {
                        Box(Modifier.size(6.dp).background(palette.fg, CircleShape))
                    }
                }
                Text(
                    name,
                    color = if (selected) palette.fg else palette.dim,
                    fontFamily = FontFamily.Monospace,
                    fontSize = 13.sp,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f),
                )
                Text(detail, color = palette.dim, fontFamily = FontFamily.Monospace, fontSize = 11.sp)
            }
        },
        onClick = onClick,
    )
}

private fun jobBarFraction(job: JobView): Float? {
    val status = job.status
    if (job.service != null && status == "fetching") {
        return job.remotePermille.toFloat() / 1000f
    }
    // Waiting phases with no meaningful percentage: an indeterminate bar.
    if (status in setOf("queued", "starting", "fetching metadata", "connecting to peers") ||
        status.contains("Initializing")
    ) {
        return null
    }
    if (job.total <= 1uL && job.downloaded > job.total) {
        return null
    }
    if (job.total == 0uL) {
        return 0f
    }
    return (job.downloaded.toDouble() / job.total.toDouble()).toFloat().coerceIn(0f, 1f)
}

private fun mix(
    a: androidx.compose.ui.graphics.Color,
    b: androidx.compose.ui.graphics.Color,
    towardB: Float,
): androidx.compose.ui.graphics.Color {
    val t = towardB.coerceIn(0f, 1f)
    return androidx.compose.ui.graphics.Color(
        red = a.red * (1f - t) + b.red * t,
        green = a.green * (1f - t) + b.green * t,
        blue = a.blue * (1f - t) + b.blue * t,
        alpha = 1f,
    )
}

private fun requestAllFiles(activity: ComponentActivity) {
    val intent =
        Intent(Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION).apply {
            data = Uri.parse("package:${activity.packageName}")
        }
    activity.startActivity(intent)
}

private fun isIgnoringBattery(activity: ComponentActivity): Boolean {
    val pm = activity.getSystemService(PowerManager::class.java)
    return pm.isIgnoringBatteryOptimizations(activity.packageName)
}

private fun requestBatteryExemption(activity: ComponentActivity) {
    val intent =
        Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS).apply {
            data = Uri.parse("package:${activity.packageName}")
        }
    runCatching { activity.startActivity(intent) }
}


/** The .torrent a VIEW intent carries ("Open with", a browser download). */
private fun torrentUri(intent: Intent?): Uri? =
    intent?.takeIf { it.action == Intent.ACTION_VIEW }?.data

/** Largest .torrent read (the engine refuses anything larger too). */
private const val MAX_TORRENT_FILE = 32 * 1024 * 1024

/** Read a .torrent from the picker or "Open with", refusing an oversize one
 *  before reading it all. */
private fun readTorrent(context: Context, uri: Uri): ByteArray {
    val input = context.contentResolver.openInputStream(uri) ?: error("could not open that file")
    input.use { stream ->
        val out = java.io.ByteArrayOutputStream()
        val buffer = ByteArray(64 * 1024)
        while (true) {
            val read = stream.read(buffer)
            if (read < 0) break
            out.write(buffer, 0, read)
            if (out.size() > MAX_TORRENT_FILE) error("the .torrent file is larger than 32 MB")
        }
        return out.toByteArray()
    }
}
