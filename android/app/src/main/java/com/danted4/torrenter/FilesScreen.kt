package com.danted4.torrenter

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import uniffi.torrenter.FileGroupView
import uniffi.torrenter.JobFilesView

/**
 * A torrent job's files in place of the job list: search, all/none, then the
 * files grouped by folder with a checkbox, size and bar each. Every tick or
 * untick is applied at once through [onSet] (the new selection, sorted ids).
 */
@Composable
fun FilesScreen(
    title: String,
    files: JobFilesView,
    palette: Palette,
    query: String,
    onQuery: (String) -> Unit,
    onSet: (List<UInt>) -> Unit,
    onBack: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val all = files.groups.flatMap { it.files }
    val chosen = all.filter { it.selected }.map { it.id }.toSet()
    val groups: List<FileGroupView> =
        files.groups.mapNotNull { group ->
            val kept = group.files.filter { fileMatches(group.folder, it.name, query) }
            if (kept.isEmpty()) null else group.copy(files = kept)
        }
    val shown = groups.flatMap { group -> group.files.map { it.id } }
    val line = mix(palette.rowAlt, palette.dim, 0.35f)
    Column(modifier = modifier.fillMaxWidth(), verticalArrangement = Arrangement.spacedBy(10.dp)) {
        PageBar(
            name = "files",
            palette = palette,
            onBack = onBack,
            detail = title,
            note = "${chosen.size} of ${all.size}",
        )
        OutlinedTextField(
            value = query,
            onValueChange = onQuery,
            modifier = Modifier.fillMaxWidth(),
            placeholder = { Text("search files", fontFamily = FontFamily.Monospace) },
            singleLine = true,
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
        val allState = checkOf(shown.count { it in chosen }, shown.size)
        Row(
            modifier =
                Modifier.clickable(enabled = shown.isNotEmpty()) {
                    onSet(toggledFiles(chosen, shown, allState != Check.ON))
                },
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            CheckGlyph(allState, palette)
            Text(
                if (query.isBlank()) "all" else "all shown",
                color = palette.dim,
                fontFamily = FontFamily.Monospace,
                fontSize = 13.sp,
            )
        }
        HorizontalDivider(color = line)
        if (groups.isEmpty()) {
            Text("no files match that search", color = palette.dim, fontFamily = FontFamily.Monospace)
            return@Column
        }
        LazyColumn(modifier = Modifier.fillMaxWidth()) {
            for (group in groups) {
                if (group.folder.isNotEmpty()) {
                    item(key = "folder:${group.folder}") {
                        val ids = group.files.map { it.id }
                        val on = group.files.count { it.selected }
                        val state = checkOf(on, group.files.size)
                        Column {
                            Row(
                                modifier =
                                    Modifier
                                        .fillMaxWidth()
                                        .clickable { onSet(toggledFiles(chosen, ids, state != Check.ON)) }
                                        .padding(top = 12.dp, bottom = 8.dp),
                                verticalAlignment = Alignment.CenterVertically,
                                horizontalArrangement = Arrangement.spacedBy(10.dp),
                            ) {
                                CheckGlyph(state, palette)
                                Text(
                                    group.folder,
                                    color = palette.fg,
                                    fontFamily = FontFamily.Monospace,
                                    fontSize = 12.sp,
                                    maxLines = 1,
                                    overflow = TextOverflow.Ellipsis,
                                    modifier = Modifier.weight(1f),
                                )
                                Text(
                                    "$on of ${group.files.size}",
                                    color = palette.dim,
                                    fontFamily = FontFamily.Monospace,
                                    fontSize = 12.sp,
                                )
                            }
                            HorizontalDivider(color = line)
                        }
                    }
                }
                items(group.files, key = { "file:${it.id}" }) { file ->
                    val fraction = fileFraction(file.have, file.size)
                    val ink = if (file.selected) palette.fg else palette.dim
                    Column {
                        Row(
                            modifier =
                                Modifier
                                    .fillMaxWidth()
                                    .clickable { onSet(toggledFiles(chosen, listOf(file.id), !file.selected)) }
                                    .padding(
                                        start = if (group.folder.isEmpty()) 0.dp else 14.dp,
                                        top = 8.dp,
                                        bottom = 8.dp,
                                    ),
                            verticalAlignment = Alignment.CenterVertically,
                            horizontalArrangement = Arrangement.spacedBy(10.dp),
                        ) {
                            CheckGlyph(if (file.selected) Check.ON else Check.OFF, palette)
                            Column(modifier = Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                                Text(
                                    file.name,
                                    color = ink,
                                    fontFamily = FontFamily.Monospace,
                                    fontSize = 13.sp,
                                    maxLines = 1,
                                    overflow = TextOverflow.Ellipsis,
                                )
                                Row(
                                    verticalAlignment = Alignment.CenterVertically,
                                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                                ) {
                                    Text(
                                        formatSize(file.size),
                                        color = palette.dim,
                                        fontFamily = FontFamily.Monospace,
                                        fontSize = 11.sp,
                                        modifier = Modifier.width(96.dp),
                                    )
                                    LinearProgressIndicator(
                                        progress = { fraction },
                                        modifier = Modifier.weight(1f),
                                        color = if (file.selected) palette.fg else mix(palette.fg, palette.dim, 0.75f),
                                        trackColor = palette.rowAlt,
                                        drawStopIndicator = {},
                                    )
                                    Text(
                                        "${(fraction * 100f).toInt()}%",
                                        color = palette.dim,
                                        fontFamily = FontFamily.Monospace,
                                        fontSize = 11.sp,
                                        modifier = Modifier.width(40.dp),
                                    )
                                }
                            }
                        }
                        HorizontalDivider(color = line)
                    }
                }
            }
            item { Spacer(Modifier.size(24.dp)) }
        }
    }
}

/** A small square checkbox in the theme's ink: a tick when on, a bar for some. */
@Composable
fun CheckGlyph(
    state: Check,
    palette: Palette,
) {
    val edge = if (state == Check.OFF) palette.dim else palette.fg
    Canvas(Modifier.size(18.dp)) {
        val u = size.minDimension
        val radius = CornerRadius(u * 0.18f, u * 0.18f)
        when (state) {
            Check.ON -> {
                drawRoundRect(palette.fg, cornerRadius = radius)
                val tick = u * 0.12f
                drawLine(palette.bg, Offset(u * 0.24f, u * 0.52f), Offset(u * 0.43f, u * 0.72f), tick, StrokeCap.Round)
                drawLine(palette.bg, Offset(u * 0.43f, u * 0.72f), Offset(u * 0.77f, u * 0.3f), tick, StrokeCap.Round)
            }
            Check.SOME -> {
                drawRoundRect(edge, cornerRadius = radius, style = Stroke(width = u * 0.09f))
                drawRoundRect(
                    palette.fg,
                    topLeft = Offset(u * 0.25f, u * 0.44f),
                    size = Size(u * 0.5f, u * 0.12f),
                    cornerRadius = CornerRadius(u * 0.06f, u * 0.06f),
                )
            }
            Check.OFF -> drawRoundRect(edge, cornerRadius = radius, style = Stroke(width = u * 0.09f))
        }
    }
}

