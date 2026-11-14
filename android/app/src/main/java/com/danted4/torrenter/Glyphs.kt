package com.danted4.torrenter

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

@Composable
fun ModeSwitch(
    magnet: Boolean,
    palette: Palette,
    onToggle: () -> Unit,
) {
    Row(
        modifier =
            Modifier
                .background(palette.row, RoundedCornerShape(2.dp))
                .border(1.dp, palette.dim, RoundedCornerShape(2.dp))
                .clickable(onClick = onToggle)
                .padding(horizontal = 10.dp, vertical = 6.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        Text(
            "MAGNET",
            color = if (magnet) palette.fg else palette.dim,
            fontFamily = FontFamily.Monospace,
            fontSize = 13.sp,
        )
        SwapArrows(palette.fg)
        Text(
            "HTTP",
            color = if (!magnet) palette.fg else palette.dim,
            fontFamily = FontFamily.Monospace,
            fontSize = 13.sp,
        )
    }
}

@Composable
fun SeedToggle(
    seed: Boolean,
    enabled: Boolean,
    palette: Palette,
    onChange: (Boolean) -> Unit,
) {
    Row(
        verticalAlignment = Alignment.Bottom,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        SeedChoice(
            share = true,
            label = "seed ON",
            selected = seed && enabled,
            faded = !enabled,
            palette = palette,
            onClick = if (enabled) ({ onChange(true) }) else null,
        )
        SeedChoice(
            share = false,
            label = "seed OFF",
            selected = !seed && enabled,
            faded = !enabled,
            palette = palette,
            onClick = if (enabled) ({ onChange(false) }) else null,
        )
    }
}

@Composable
private fun SeedChoice(
    share: Boolean,
    label: String,
    selected: Boolean,
    faded: Boolean,
    palette: Palette,
    onClick: (() -> Unit)?,
) {
    val color =
        when {
            faded -> palette.dim.copy(alpha = 70f / 255f)
            selected -> palette.fg
            else -> palette.dim
        }
    Column(
        horizontalAlignment = Alignment.CenterHorizontally,
        modifier = Modifier.then(if (onClick != null) Modifier.clickable(onClick = onClick) else Modifier),
    ) {
        SeedGlyph(
            share = share,
            selected = selected,
            faded = faded,
            palette = palette,
            glyphSize = 13.dp,
        )
        Text(
            label,
            color = color,
            fontFamily = FontFamily.Monospace,
            fontSize = 8.sp,
            lineHeight = 9.sp,
        )
    }
}

@Composable
fun SeedGlyph(
    share: Boolean,
    selected: Boolean,
    faded: Boolean,
    palette: Palette,
    onClick: (() -> Unit)? = null,
    glyphSize: Dp = 16.dp,
) {
    val color =
        when {
            faded -> palette.dim.copy(alpha = 70f / 255f)
            selected -> palette.fg
            else -> palette.dim
        }
    val box =
        Modifier
            .padding(2.dp)
            .then(
                if (selected && !faded) {
                    Modifier.border(1.2.dp, color, RoundedCornerShape(2.dp))
                } else {
                    Modifier
                },
            ).padding(2.dp)
            .size(glyphSize)
            .then(if (onClick != null) Modifier.clickable(onClick = onClick) else Modifier)
    Box(modifier = box, contentAlignment = Alignment.Center) {
        Canvas(Modifier.size(glyphSize)) {
            val stroke = Stroke(width = 1.2.dp.toPx(), cap = StrokeCap.Round)
            val c = Offset(this.size.width / 2f, this.size.height / 2f)
            val unit = this.size.width / 16f
            if (share) {
                drawCircle(color = color, radius = 4.4f * unit, center = Offset(c.x, c.y + 1.5f * unit), style = stroke)
                val halo = Path()
                for (i in 0..10) {
                    val x = -5f + i
                    val y = -5.4f + (x * x) / 16f
                    val p = Offset(c.x + x * 0.85f * unit, c.y + y * unit)
                    if (i == 0) halo.moveTo(p.x, p.y) else halo.lineTo(p.x, p.y)
                }
                drawPath(halo, color = color, style = stroke)
            } else {
                drawCircle(color = color, radius = 4.4f * unit, center = Offset(c.x, c.y + 1.8f * unit), style = stroke)
                drawLine(
                    color,
                    Offset(c.x - 2.4f * unit, c.y - 1.6f * unit),
                    Offset(c.x - 4.6f * unit, c.y - 6.4f * unit),
                    strokeWidth = stroke.width,
                    cap = StrokeCap.Round,
                )
                drawLine(
                    color,
                    Offset(c.x + 2.4f * unit, c.y - 1.6f * unit),
                    Offset(c.x + 4.6f * unit, c.y - 6.4f * unit),
                    strokeWidth = stroke.width,
                    cap = StrokeCap.Round,
                )
            }
        }
    }
}

@Composable
fun ThemeSwatches(
    selected: ThemeId,
    onSelect: (ThemeId) -> Unit,
) {
    Row(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically) {
        ThemeId.entries.forEach { id ->
            val fill = palette(id).fg
            val selectedHere = id == selected
            Box(
                modifier =
                    Modifier
                        .padding(2.dp)
                        .then(
                            if (selectedHere) {
                                Modifier.border(1.2.dp, fill, RoundedCornerShape(2.dp))
                            } else {
                                Modifier
                            },
                        ).padding(2.dp)
                        .size(16.dp)
                        .background(fill, RoundedCornerShape(2.dp))
                        .clickable { onSelect(id) },
            )
        }
    }
}

@Composable
private fun SwapArrows(color: Color) {
    Canvas(Modifier.size(22.dp, 16.dp)) {
        val stroke = Stroke(width = 1.6.dp.toPx(), cap = StrokeCap.Round)
        val c = Offset(size.width / 2f, size.height / 2f)
        val w = 7f
        val head = 3.2f
        val topY = c.y - 3.6f
        val botY = c.y + 3.6f
        val left = c.x - w
        val right = c.x + w
        drawLine(color, Offset(left, topY), Offset(right, topY), stroke.width, StrokeCap.Round)
        drawLine(color, Offset(right - head, topY - head), Offset(right, topY), stroke.width, StrokeCap.Round)
        drawLine(color, Offset(right - head, topY + head), Offset(right, topY), stroke.width, StrokeCap.Round)
        drawLine(color, Offset(right, botY), Offset(left, botY), stroke.width, StrokeCap.Round)
        drawLine(color, Offset(left + head, botY - head), Offset(left, botY), stroke.width, StrokeCap.Round)
        drawLine(color, Offset(left + head, botY + head), Offset(left, botY), stroke.width, StrokeCap.Round)
    }
}

/** Header cog: opens the settings screen. */
@Composable
fun CogGlyph(
    color: Color,
    onClick: () -> Unit,
) {
    Canvas(
        Modifier
            .size(28.dp)
            .clickable(onClick = onClick),
    ) {
        val c = Offset(size.width / 2f, size.height / 2f)
        val unit = size.minDimension / 28f
        val stroke = 1.6f * unit * 1.4f
        drawCircle(color, radius = 4.8f * unit * 1.2f, center = c, style = Stroke(width = stroke))
        drawCircle(color, radius = 1.6f * unit * 1.2f, center = c)
        for (step in 0 until 6) {
            val rad = Math.toRadians(step * 60.0)
            val dx = kotlin.math.cos(rad).toFloat()
            val dy = kotlin.math.sin(rad).toFloat()
            drawLine(
                color,
                Offset(c.x + dx * 6.4f * unit * 1.2f, c.y + dy * 6.4f * unit * 1.2f),
                Offset(c.x + dx * 8.8f * unit * 1.2f, c.y + dy * 8.8f * unit * 1.2f),
                stroke,
                StrokeCap.Round,
            )
        }
    }
}

/** "debrid" (off) or "via TorBox" (on); a tap cycles through services with a key. */
@Composable
fun DebridControl(
    label: String?,
    palette: Palette,
    onClick: () -> Unit,
) {
    val on = label != null
    Text(
        if (on) "via $label" else "debrid",
        color = if (on) palette.fg else palette.dim,
        fontFamily = FontFamily.Monospace,
        fontSize = 13.sp,
        modifier =
            Modifier
                .border(1.dp, if (on) palette.fg else palette.dim, RoundedCornerShape(4.dp))
                .clickable(onClick = onClick)
                .padding(horizontal = 10.dp, vertical = 6.dp),
    )
}

/** Small selectable text chip (settings choices). */
@Composable
fun Chip(
    text: String,
    on: Boolean,
    palette: Palette,
    onClick: () -> Unit,
) {
    Text(
        text,
        color = if (on) palette.fg else palette.dim,
        fontFamily = FontFamily.Monospace,
        fontSize = 13.sp,
        modifier =
            Modifier
                .then(if (on) Modifier.border(1.dp, palette.fg, RoundedCornerShape(4.dp)) else Modifier)
                .clickable(onClick = onClick)
                .padding(horizontal = 10.dp, vertical = 5.dp),
    )
}
