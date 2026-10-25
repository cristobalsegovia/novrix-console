import type { ITheme } from "@xterm/xterm";

export type ThemeMode = "dark" | "light";

const STORAGE_KEY = "novrix-console-theme";

export function initialTheme(): ThemeMode {
  const saved = localStorage.getItem(STORAGE_KEY);
  if (saved === "light" || saved === "dark") return saved;
  return window.matchMedia("(prefers-color-scheme: light)").matches
    ? "light"
    : "dark";
}

export function persistTheme(mode: ThemeMode) {
  localStorage.setItem(STORAGE_KEY, mode);
}

export const TERM_THEMES: Record<ThemeMode, ITheme> = {
  dark: {
    background: "#0E0F12",
    foreground: "#E6E8EC",
    cursor: "#7A97FF",
    cursorAccent: "#0E0F12",
    selectionBackground: "rgba(36, 87, 245, 0.34)",
    black: "#171A20",
    red: "#FF6B5E",
    green: "#3FB97A",
    yellow: "#E0A040",
    blue: "#7A97FF",
    magenta: "#C084FC",
    cyan: "#38BDF8",
    white: "#E6E8EC",
    brightBlack: "#6B7280",
    brightRed: "#FF8A80",
    brightGreen: "#5CE096",
    brightYellow: "#FBBF24",
    brightBlue: "#9DB2FF",
    brightMagenta: "#D8B4FE",
    brightCyan: "#7DD3FC",
    brightWhite: "#FFFFFF",
  },
  light: {
    background: "#FFFFFF",
    foreground: "#15171C",
    cursor: "#2457F5",
    cursorAccent: "#FFFFFF",
    selectionBackground: "rgba(36, 87, 245, 0.18)",
    black: "#15171C",
    red: "#C2291F",
    green: "#1F7A47",
    yellow: "#9A5A00",
    blue: "#2457F5",
    magenta: "#7E22CE",
    cyan: "#0E7490",
    white: "#F4F5F7",
    brightBlack: "#5A616D",
    brightRed: "#DC2626",
    brightGreen: "#16A34A",
    brightYellow: "#CA8A04",
    brightBlue: "#1B45CC",
    brightMagenta: "#9333EA",
    brightCyan: "#0891B2",
    brightWhite: "#FFFFFF",
  },
};
