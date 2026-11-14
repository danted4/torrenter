package com.danted4.torrenter

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonColors
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import uniffi.torrenter.debridServices

/**
 * Full settings screen in place of the main screen (system back or "← back"
 * returns). Downloads keep running underneath.
 */
@Composable
fun SettingsScreen(
    palette: Palette,
    themeId: ThemeId,
    onTheme: (ThemeId) -> Unit,
    buttons: ButtonColors,
    keyed: Set<String>,
    notes: Map<String, String>,
    connections: Int,
    onConnections: (Int) -> Unit,
    removeAfter: Boolean,
    onRemoveAfter: (Boolean) -> Unit,
    onSaveKey: (String, String) -> Unit,
    onTestKey: (String) -> Unit,
    onRemoveKey: (String) -> Unit,
    onBack: () -> Unit,
) {
    val drafts = remember { mutableStateMapOf<String, String>() }
    Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
        Row(
            modifier = Modifier.fillMaxWidth(),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.SpaceBetween,
        ) {
            Text(
                "← back",
                color = palette.fg,
                fontFamily = FontFamily.Monospace,
                modifier = Modifier.clickable(onClick = onBack).padding(4.dp),
            )
            Text("settings", color = palette.fg, fontFamily = FontFamily.Monospace, fontSize = 18.sp)
        }
        Column(
            modifier = Modifier.verticalScroll(rememberScrollState()),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Heading("appearance", palette)
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                Text("theme", color = palette.dim, fontFamily = FontFamily.Monospace, fontSize = 12.sp)
                ThemeSwatches(selected = themeId, onSelect = onTheme)
            }

            Heading("debrid accounts", palette)
            Text(
                "Uses your own account. The provider's terms apply.",
                color = palette.dim,
                fontFamily = FontFamily.Monospace,
                fontSize = 12.sp,
            )
            debridServices().forEach { service ->
                val has = service.id in keyed
                Column(
                    modifier =
                        Modifier
                            .fillMaxWidth()
                            .background(palette.row, RoundedCornerShape(4.dp))
                            .padding(10.dp),
                    verticalArrangement = Arrangement.spacedBy(6.dp),
                ) {
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        Text(service.label, color = palette.fg, fontFamily = FontFamily.Monospace, fontSize = 14.sp)
                        Text(
                            if (has) "key saved" else "no key",
                            color = if (has) palette.fg else palette.dim,
                            fontFamily = FontFamily.Monospace,
                            fontSize = 12.sp,
                        )
                    }
                    notes[service.id]?.let {
                        Text(it, color = palette.dim, fontFamily = FontFamily.Monospace, fontSize = 12.sp)
                    }
                    val draft = drafts[service.id].orEmpty()
                    OutlinedTextField(
                        value = draft,
                        onValueChange = { drafts[service.id] = it },
                        modifier = Modifier.fillMaxWidth(),
                        placeholder = { Text("paste API key", fontFamily = FontFamily.Monospace) },
                        singleLine = true,
                        visualTransformation = PasswordVisualTransformation(),
                        textStyle =
                            androidx.compose.ui.text.TextStyle(
                                color = palette.fg,
                                fontFamily = FontFamily.Monospace,
                                fontSize = 13.sp,
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
                                focusedContainerColor = palette.rowAlt,
                                unfocusedContainerColor = palette.rowAlt,
                            ),
                        shape = RoundedCornerShape(2.dp),
                    )
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        if (draft.isNotBlank()) {
                            Button(
                                onClick = {
                                    onSaveKey(service.id, draft.trim())
                                    drafts.remove(service.id)
                                },
                                colors = buttons,
                                shape = RoundedCornerShape(2.dp),
                            ) { Text("save", fontFamily = FontFamily.Monospace) }
                        }
                        if (has) {
                            Button(onClick = { onTestKey(service.id) }, colors = buttons, shape = RoundedCornerShape(2.dp)) {
                                Text("test", fontFamily = FontFamily.Monospace)
                            }
                            Button(onClick = { onRemoveKey(service.id) }, colors = buttons, shape = RoundedCornerShape(2.dp)) {
                                Text("remove", fontFamily = FontFamily.Monospace)
                            }
                        }
                    }
                }
            }

            Heading("downloads", palette)
            Text("connections per download", color = palette.dim, fontFamily = FontFamily.Monospace, fontSize = 12.sp)
            Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                Chip("auto", connections == 0, palette) { onConnections(0) }
                listOf(1, 2, 4, 8, 16).forEach { parts ->
                    Chip("$parts", connections == parts, palette) { onConnections(parts) }
                }
            }
            Text(
                "new HTTP and debrid downloads; each service caps it",
                color = palette.dim,
                fontFamily = FontFamily.Monospace,
                fontSize = 11.sp,
            )
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(
                    "remove from service after download",
                    color = if (removeAfter) palette.fg else palette.dim,
                    fontFamily = FontFamily.Monospace,
                    fontSize = 13.sp,
                )
                Chip(if (removeAfter) "on" else "off", removeAfter, palette) { onRemoveAfter(!removeAfter) }
            }
            Spacer(Modifier.height(24.dp))
        }
    }
}

@Composable
private fun Heading(
    text: String,
    palette: Palette,
) {
    Column(verticalArrangement = Arrangement.spacedBy(4.dp), modifier = Modifier.padding(top = 10.dp)) {
        Text(text, color = palette.fg, fontFamily = FontFamily.Monospace, fontSize = 15.sp)
        Spacer(
            Modifier
                .fillMaxWidth()
                .height(1.dp)
                .background(palette.dim),
        )
    }
}
