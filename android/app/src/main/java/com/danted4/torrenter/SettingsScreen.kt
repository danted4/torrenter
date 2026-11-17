package com.danted4.torrenter

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonColors
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import uniffi.torrenter.debridServices

/** Settings categories, shown as chips across the top (the desktop sidebar). */
enum class SettingsTab(
    val label: String,
) {
    APPEARANCE("appearance"),
    DOWNLOADS("downloads"),
    ACCOUNTS("debrid accounts"),
}

/** A new account being added: provider first, then a name and the key. */
private data class AddAccountForm(
    val service: String? = null,
    val name: String = "",
    val key: String = "",
)

/**
 * Full settings screen in place of the main screen (the header's cog turns
 * into a back arrow; system back returns too). Same layout as the desktop:
 * a category picker, then tiles with the setting on the left and its control
 * on the right. Downloads keep running underneath.
 */
@Composable
fun SettingsScreen(
    palette: Palette,
    themeId: ThemeId,
    onTheme: (ThemeId) -> Unit,
    buttons: ButtonColors,
    tab: SettingsTab,
    onTab: (SettingsTab) -> Unit,
    accounts: List<DebridAccount>,
    notes: Map<String, String>,
    connections: Int,
    onConnections: (Int) -> Unit,
    removeAfter: Boolean,
    onRemoveAfter: (Boolean) -> Unit,
    onAddAccount: (service: String, name: String, key: String) -> Boolean,
    onTestAccount: (String) -> Unit,
    onRemoveAccount: (String) -> Unit,
) {
    var form by remember { mutableStateOf<AddAccountForm?>(null) }
    Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
        Row(
            modifier = Modifier.horizontalScroll(rememberScrollState()),
            horizontalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            SettingsTab.entries.forEach { each ->
                Chip(each.label, tab == each, palette) { onTab(each) }
            }
        }
        Column(
            modifier = Modifier.verticalScroll(rememberScrollState()),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            when (tab) {
                SettingsTab.APPEARANCE -> {
                    PageTitle("appearance", palette)
                    Tile("theme", "colour theme for every screen", palette) {
                        ThemeSwatches(selected = themeId, onSelect = onTheme)
                    }
                }
                SettingsTab.DOWNLOADS -> {
                    PageTitle("downloads", palette)
                    Tile(
                        "connections per download",
                        "parallel connections for new HTTP and debrid downloads; auto is 4, and each provider caps it",
                        palette,
                        stacked = true,
                    ) {
                        Row(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                            Chip("auto", connections == 0, palette) { onConnections(0) }
                            listOf(1, 2, 4, 8, 16).forEach { parts ->
                                Chip("$parts", connections == parts, palette) { onConnections(parts) }
                            }
                        }
                    }
                    Tile(
                        "remove from debrid account after download",
                        "once every file has arrived; only torrents torrenter added",
                        palette,
                    ) {
                        Switch(on = removeAfter, palette = palette) { onRemoveAfter(!removeAfter) }
                    }
                }
                SettingsTab.ACCOUNTS -> {
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Box(Modifier.weight(1f)) { PageTitle("debrid accounts", palette) }
                        if (form == null) {
                            SmallButton("+ add account", palette.fg, buttons) { form = AddAccountForm() }
                        }
                    }
                    form?.let { current ->
                        AddAccountCard(
                            form = current,
                            accounts = accounts,
                            palette = palette,
                            buttons = buttons,
                            onChange = { form = it },
                            onCancel = { form = null },
                            onSave = { service, name, key ->
                                if (onAddAccount(service, name, key)) form = null
                            },
                        )
                    }
                    if (accounts.isEmpty() && form == null) {
                        Column(
                            modifier = Modifier.fillMaxWidth().background(palette.row, RoundedCornerShape(4.dp)).padding(18.dp),
                            horizontalAlignment = Alignment.CenterHorizontally,
                            verticalArrangement = Arrangement.spacedBy(4.dp),
                        ) {
                            Text("no accounts yet", color = palette.fg, fontFamily = FontFamily.Monospace, fontSize = 13.sp)
                            Text(
                                "add one to download magnets through a debrid service instead of from peers",
                                color = palette.dim,
                                fontFamily = FontFamily.Monospace,
                                fontSize = 12.sp,
                                textAlign = TextAlign.Center,
                            )
                        }
                    }
                    accounts.forEach { account ->
                        val provider = serviceMenuLabel(account.service)
                        val detail = notes[account.id]?.let { "$provider · $it" } ?: "$provider · key saved"
                        Row(
                            modifier = Modifier.fillMaxWidth().background(palette.row, RoundedCornerShape(4.dp)).padding(12.dp),
                            verticalAlignment = Alignment.CenterVertically,
                            horizontalArrangement = Arrangement.spacedBy(8.dp),
                        ) {
                            Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(3.dp)) {
                                Text(account.name, color = palette.fg, fontFamily = FontFamily.Monospace, fontSize = 14.sp, maxLines = 1)
                                Text(detail, color = palette.dim, fontFamily = FontFamily.Monospace, fontSize = 11.sp)
                            }
                            SmallButton("test", palette.fg, buttons) { onTestAccount(account.id) }
                            SmallButton("remove", palette.fail, buttons) { onRemoveAccount(account.id) }
                        }
                    }
                    Text(
                        "uses your own account; the provider's terms apply",
                        color = palette.dim,
                        fontFamily = FontFamily.Monospace,
                        fontSize = 11.sp,
                    )
                }
            }
            Spacer(Modifier.height(24.dp))
        }
    }
}

@Composable
private fun AddAccountCard(
    form: AddAccountForm,
    accounts: List<DebridAccount>,
    palette: Palette,
    buttons: ButtonColors,
    onChange: (AddAccountForm) -> Unit,
    onCancel: () -> Unit,
    onSave: (String, String, String) -> Unit,
) {
    val typed = form.name.trim()
    val nameError =
        if (typed.isNotEmpty() && Accounts.nameTaken(typed, accounts)) "\"$typed\" is already one of your accounts" else null
    val ready = form.service != null && form.key.isNotBlank() && nameError == null
    Column(
        modifier = Modifier.fillMaxWidth().background(palette.row, RoundedCornerShape(4.dp)).padding(12.dp),
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Column(verticalArrangement = Arrangement.spacedBy(3.dp)) {
            Text("add account", color = palette.fg, fontFamily = FontFamily.Monospace, fontSize = 13.sp)
            Text(
                "paste an API key from your provider's account page; it is stored only on this device",
                color = palette.dim,
                fontFamily = FontFamily.Monospace,
                fontSize = 11.sp,
            )
        }
        // The fields sit in an inset panel the header owns.
        Column(
            modifier = Modifier.fillMaxWidth().background(palette.bg.copy(alpha = 0.7f), RoundedCornerShape(6.dp)).padding(10.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            val providerNote =
                if (serviceExperimental(form.service)) {
                    "experimental: not yet tested with a real account; please report problems"
                } else {
                    "the debrid service the key belongs to"
                }
            FormRow("provider", providerNote, palette) {
                ProviderSelect(form.service, palette) { onChange(form.copy(service = it)) }
            }
            form.service?.let { service ->
                Divider(palette)
                FormRow(
                    "name",
                    nameError ?: "shown in the download menu; optional",
                    palette,
                    tint = if (nameError == null) palette.dim else palette.fail,
                ) {
                    FormField(
                        value = form.name,
                        onValue = { onChange(form.copy(name = it)) },
                        placeholder = Accounts.defaultName(serviceLabel(service), accounts),
                        secret = false,
                        error = nameError != null,
                        palette = palette,
                    )
                }
                Divider(palette)
                FormRow("api key", "from your ${serviceLabel(service)} account settings", palette) {
                    FormField(
                        value = form.key,
                        onValue = { onChange(form.copy(key = it)) },
                        placeholder = "paste the key",
                        secret = true,
                        error = false,
                        palette = palette,
                    )
                }
            }
        }
        // Both actions together, centred: cancel, then the primary save.
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(8.dp, Alignment.CenterHorizontally),
        ) {
            SmallButton("cancel", palette.fail, buttons, onClick = onCancel)
            SmallButton(
                "save account",
                if (ready) palette.fg else palette.dim,
                buttons,
                enabled = ready,
                outlined = ready,
            ) {
                form.service?.let { onSave(it, form.name, form.key) }
            }
        }
    }
}

/** The provider select: full width, label centred, chevron on the right. */
@Composable
private fun ProviderSelect(
    service: String?,
    palette: Palette,
    onPick: (String) -> Unit,
) {
    var open by remember { mutableStateOf(false) }
    // The list matches the select's measured width, like the desktop.
    var anchorWidth by remember { mutableStateOf(0.dp) }
    val density = LocalDensity.current
    Box {
        Box(
            modifier =
                Modifier
                    .fillMaxWidth()
                    .height(40.dp)
                    .onGloballyPositioned { anchorWidth = with(density) { it.size.width.toDp() } }
                    .border(1.dp, if (service == null) palette.dim else palette.fg, RoundedCornerShape(2.dp))
                    .clickable { open = true },
            contentAlignment = Alignment.Center,
        ) {
            Text(
                service?.let { serviceMenuLabel(it) } ?: "choose a provider",
                color = if (service == null) palette.dim else palette.fg,
                fontFamily = FontFamily.Monospace,
                fontSize = 13.sp,
                maxLines = 1,
                modifier = Modifier.padding(horizontal = 24.dp),
            )
            Text(
                "▾",
                color = if (service == null) palette.dim else palette.fg,
                fontSize = 13.sp,
                modifier = Modifier.align(Alignment.CenterEnd).padding(end = 10.dp),
            )
        }
        DropdownMenu(
            expanded = open,
            onDismissRequest = { open = false },
            modifier = Modifier.background(palette.row).width(anchorWidth),
        ) {
            debridServices().forEach { each ->
                DropdownMenuItem(
                    text = {
                        Text(
                            if (each.experimental) "${each.label} (experimental)" else each.label,
                            color = if (service == each.id) palette.fg else palette.dim,
                            fontFamily = FontFamily.Monospace,
                            fontSize = 13.sp,
                            textAlign = TextAlign.Center,
                            modifier = Modifier.fillMaxWidth(),
                        )
                    },
                    onClick = {
                        open = false
                        onPick(each.id)
                    },
                )
            }
        }
    }
}

/** A text field styled like the URL field: placeholder centred, typed text
 *  from the left; a fail-coloured border while `error` is set. */
@Composable
private fun FormField(
    value: String,
    onValue: (String) -> Unit,
    placeholder: String,
    secret: Boolean,
    error: Boolean,
    palette: Palette,
) {
    val border = if (error) palette.fail else palette.dim
    OutlinedTextField(
        value = value,
        onValueChange = onValue,
        modifier = Modifier.fillMaxWidth(),
        placeholder = {
            Text(
                placeholder,
                fontFamily = FontFamily.Monospace,
                fontSize = 13.sp,
                textAlign = TextAlign.Center,
                modifier = Modifier.fillMaxWidth(),
            )
        },
        singleLine = true,
        visualTransformation = if (secret) PasswordVisualTransformation() else VisualTransformation.None,
        textStyle = TextStyle(color = palette.fg, fontFamily = FontFamily.Monospace, fontSize = 13.sp),
        colors =
            OutlinedTextFieldDefaults.colors(
                focusedBorderColor = if (error) palette.fail else palette.fg,
                unfocusedBorderColor = border,
                focusedTextColor = palette.fg,
                unfocusedTextColor = palette.fg,
                cursorColor = palette.fg,
                focusedPlaceholderColor = palette.dim,
                unfocusedPlaceholderColor = palette.dim,
                focusedContainerColor = palette.bg,
                unfocusedContainerColor = palette.bg,
            ),
        shape = RoundedCornerShape(2.dp),
    )
}

/** A form row on a phone: title and description, the control under them. */
@Composable
private fun FormRow(
    title: String,
    description: String,
    palette: Palette,
    tint: Color = palette.dim,
    control: @Composable () -> Unit,
) {
    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
        Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
            Text(title, color = palette.fg, fontFamily = FontFamily.Monospace, fontSize = 13.sp)
            Text(description, color = tint, fontFamily = FontFamily.Monospace, fontSize = 11.sp)
        }
        control()
    }
}

/** One setting as a full-width tile: title and description on the left, the
 *  control on the right (or under them when `stacked`, for a wide control). */
@Composable
private fun Tile(
    title: String,
    description: String,
    palette: Palette,
    stacked: Boolean = false,
    control: @Composable () -> Unit,
) {
    val text: @Composable (Modifier) -> Unit = { modifier ->
        Column(modifier, verticalArrangement = Arrangement.spacedBy(3.dp)) {
            Text(title, color = palette.fg, fontFamily = FontFamily.Monospace, fontSize = 13.sp)
            Text(description, color = palette.dim, fontFamily = FontFamily.Monospace, fontSize = 11.sp)
        }
    }
    val card = Modifier.fillMaxWidth().background(palette.row, RoundedCornerShape(4.dp)).padding(12.dp)
    if (stacked) {
        Column(card, verticalArrangement = Arrangement.spacedBy(10.dp)) {
            text(Modifier)
            Box(Modifier.fillMaxWidth(), contentAlignment = Alignment.CenterEnd) { control() }
        }
    } else {
        Row(card, verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            text(Modifier.weight(1f))
            control()
        }
    }
}

@Composable
private fun PageTitle(
    text: String,
    palette: Palette,
) {
    Text(text, color = palette.fg, fontFamily = FontFamily.Monospace, fontSize = 15.sp, modifier = Modifier.padding(top = 2.dp))
}

@Composable
private fun Divider(palette: Palette) {
    Spacer(Modifier.fillMaxWidth().height(1.dp).background(palette.dim.copy(alpha = 0.4f)))
}

/** A compact switch matching the desktop / iOS one. */
@Composable
private fun Switch(
    on: Boolean,
    palette: Palette,
    onClick: () -> Unit,
) {
    Box(
        modifier =
            Modifier
                .size(width = 34.dp, height = 18.dp)
                .background(if (on) palette.fg else Color.Transparent, RoundedCornerShape(9.dp))
                .border(1.dp, if (on) palette.fg else palette.dim, RoundedCornerShape(9.dp))
                .clickable(onClick = onClick)
                .padding(horizontal = 3.dp),
        contentAlignment = if (on) Alignment.CenterEnd else Alignment.CenterStart,
    ) {
        Box(Modifier.size(11.dp).background(if (on) palette.bg else palette.dim, CircleShape))
    }
}

/** A small monospace button; `color` tints its text (remove / cancel use
 *  fail), `outlined` adds a bright border for the primary action. */
@Composable
fun SmallButton(
    text: String,
    color: Color,
    buttons: ButtonColors,
    enabled: Boolean = true,
    outlined: Boolean = false,
    onClick: () -> Unit,
) {
    Button(
        onClick = onClick,
        enabled = enabled,
        colors =
            ButtonDefaults.buttonColors(
                containerColor = buttons.containerColor,
                contentColor = color,
                disabledContainerColor = buttons.containerColor,
                disabledContentColor = color,
            ),
        shape = RoundedCornerShape(2.dp),
        border = if (outlined) androidx.compose.foundation.BorderStroke(1.dp, color) else null,
        contentPadding = androidx.compose.foundation.layout.PaddingValues(horizontal = 10.dp, vertical = 4.dp),
    ) { Text(text, fontFamily = FontFamily.Monospace, fontSize = 13.sp) }
}
