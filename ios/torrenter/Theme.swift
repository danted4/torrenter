import SwiftUI

enum ThemeId: String, CaseIterable {
    case phosphor
    case amber
    case ice
    case scarlet
    case violet

    static func fromName(_ name: String) -> ThemeId {
        ThemeId(rawValue: name.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()) ?? .phosphor
    }
}

struct Palette {
    let bg: Color
    let row: Color
    let rowAlt: Color
    let fg: Color
    let dim: Color
    let fail: Color
}

func themePalette(_ id: ThemeId) -> Palette {
    switch id {
    case .phosphor:
        Palette(
            bg: Color(red: 0x0A / 255, green: 0x0A / 255, blue: 0x0A / 255),
            row: Color(red: 0x10 / 255, green: 0x10 / 255, blue: 0x10 / 255),
            rowAlt: Color(red: 0x16 / 255, green: 0x16 / 255, blue: 0x16 / 255),
            fg: Color(red: 0x6E / 255, green: 1, blue: 0x6E / 255),
            dim: Color(red: 0x3E / 255, green: 0x8C / 255, blue: 0x3E / 255),
            fail: Color(red: 1, green: 0x5A / 255, blue: 0x5A / 255)
        )
    case .amber:
        Palette(
            bg: Color(red: 0x0C / 255, green: 0x09 / 255, blue: 0x04 / 255),
            row: Color(red: 0x16 / 255, green: 0x10 / 255, blue: 0x06 / 255),
            rowAlt: Color(red: 0x1E / 255, green: 0x16 / 255, blue: 0x08 / 255),
            fg: Color(red: 1, green: 0xB8 / 255, blue: 0x48 / 255),
            dim: Color(red: 0xA8 / 255, green: 0x70 / 255, blue: 0x20 / 255),
            fail: Color(red: 1, green: 0x60 / 255, blue: 0x40 / 255)
        )
    case .ice:
        Palette(
            bg: Color(red: 0x06 / 255, green: 0x0C / 255, blue: 0x12 / 255),
            row: Color(red: 0x0A / 255, green: 0x14 / 255, blue: 0x1E / 255),
            rowAlt: Color(red: 0x0E / 255, green: 0x1C / 255, blue: 0x28 / 255),
            fg: Color(red: 0x8C / 255, green: 0xDC / 255, blue: 1),
            dim: Color(red: 0x46 / 255, green: 0x78 / 255, blue: 0x96 / 255),
            fail: Color(red: 1, green: 0x78 / 255, blue: 0x8C / 255)
        )
    case .scarlet:
        Palette(
            bg: Color(red: 0x0A / 255, green: 0x06 / 255, blue: 0x08 / 255),
            row: Color(red: 0x14 / 255, green: 0x0A / 255, blue: 0x0E / 255),
            rowAlt: Color(red: 0x1C / 255, green: 0x0E / 255, blue: 0x12 / 255),
            fg: Color(red: 1, green: 0x58 / 255, blue: 0x66 / 255),
            dim: Color(red: 0xB0 / 255, green: 0x34 / 255, blue: 0x40 / 255),
            fail: Color(red: 1, green: 0xA8 / 255, blue: 0x48 / 255)
        )
    case .violet:
        Palette(
            bg: Color(red: 0x0A / 255, green: 0x08 / 255, blue: 0x0E / 255),
            row: Color(red: 0x12 / 255, green: 0x0E / 255, blue: 0x1A / 255),
            rowAlt: Color(red: 0x1A / 255, green: 0x14 / 255, blue: 0x24 / 255),
            fg: Color(red: 0xD2 / 255, green: 0xBA / 255, blue: 1),
            dim: Color(red: 0x94 / 255, green: 0x7C / 255, blue: 0xC4 / 255),
            fail: Color(red: 1, green: 0x8C / 255, blue: 0x60 / 255)
        )
    }
}

enum Prefs {
    static let themeKey = "theme"

    static func theme() -> ThemeId {
        ThemeId.fromName(UserDefaults.standard.string(forKey: themeKey) ?? ThemeId.phosphor.rawValue)
    }

    static func saveTheme(_ id: ThemeId) {
        UserDefaults.standard.set(id.rawValue, forKey: themeKey)
    }

    static let debridRouteKey = "debridRoute"
    static let connectionsKey = "connections"
    static let removeAfterKey = "debridRemoveAfter"

    /// Remembered debrid service for magnet jobs; nil is off.
    static func debridRoute() -> String? {
        UserDefaults.standard.string(forKey: debridRouteKey)
    }

    static func saveDebridRoute(_ id: String?) {
        if let id {
            UserDefaults.standard.set(id, forKey: debridRouteKey)
        } else {
            UserDefaults.standard.removeObject(forKey: debridRouteKey)
        }
    }

    /// Connections per download; 0 is Auto (4).
    static func connections() -> Int {
        UserDefaults.standard.integer(forKey: connectionsKey)
    }

    static func saveConnections(_ parts: Int) {
        UserDefaults.standard.set(parts, forKey: connectionsKey)
    }

    static func removeAfter() -> Bool {
        UserDefaults.standard.bool(forKey: removeAfterKey)
    }

    static func saveRemoveAfter(_ on: Bool) {
        UserDefaults.standard.set(on, forKey: removeAfterKey)
    }

    static let keepAliveKey = "keepAlive"
    static let parkedKey = "parkedJobs"

    static func keepAlive() -> Bool {
        UserDefaults.standard.bool(forKey: keepAliveKey)
    }

    static func saveKeepAlive(_ on: Bool) {
        UserDefaults.standard.set(on, forKey: keepAliveKey)
    }

    /// Job ids stopped by leaving the app, restarted on the next foreground.
    static func parked() -> [UInt64] {
        (UserDefaults.standard.array(forKey: parkedKey) as? [String] ?? []).compactMap(UInt64.init)
    }

    static func saveParked(_ ids: [UInt64]) {
        if ids.isEmpty {
            UserDefaults.standard.removeObject(forKey: parkedKey)
        } else {
            UserDefaults.standard.set(ids.map(String.init), forKey: parkedKey)
        }
    }
}
