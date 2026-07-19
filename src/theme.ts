import type { ITheme } from "@xterm/xterm";

export type ThemeMode = "dark" | "light";

/** xterm palettes matched to the CSS variables in styles.css. */
export const TERM_THEMES: Record<ThemeMode, ITheme> = {
  dark: {
    background: "#0f1418",
    foreground: "#e8ecf1",
    cursor: "#4fb3e8",
    cursorAccent: "#0f1418",
    selectionBackground: "#4fb3e844",
    black: "#1c242c",
    red: "#e15b4e",
    green: "#45c46b",
    yellow: "#e8b44f",
    blue: "#4fb3e8",
    magenta: "#b08ae0",
    cyan: "#4ecdc4",
    white: "#e8ecf1",
    brightBlack: "#5c6a76",
    brightRed: "#f07f70",
    brightGreen: "#63d986",
    brightYellow: "#f2c775",
    brightBlue: "#7cc7f2",
    brightMagenta: "#c7a6ee",
    brightCyan: "#72ddd5",
    brightWhite: "#f4f7fa",
  },
  light: {
    background: "#ffffff",
    foreground: "#1f2937",
    cursor: "#1e88c7",
    cursorAccent: "#ffffff",
    selectionBackground: "#4fb3e833",
    black: "#3d4852",
    red: "#cc3d2f",
    green: "#1f8a44",
    yellow: "#9a6b0a",
    blue: "#1e6fae",
    magenta: "#7a4bad",
    cyan: "#0e7d76",
    white: "#8a97a3",
    brightBlack: "#66727d",
    brightRed: "#e04d3d",
    brightGreen: "#249c50",
    brightYellow: "#b57e0c",
    brightBlue: "#1e88c7",
    brightMagenta: "#8d5cc4",
    brightCyan: "#0f938a",
    brightWhite: "#2b3540",
  },
};

const KEY = "sb-theme";

export function initialTheme(): ThemeMode {
  const saved = localStorage.getItem(KEY);
  if (saved === "light" || saved === "dark") return saved;
  return window.matchMedia("(prefers-color-scheme: light)").matches
    ? "light"
    : "dark";
}

export function persistTheme(mode: ThemeMode) {
  localStorage.setItem(KEY, mode);
}
