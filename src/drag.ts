import type { MouseEvent } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";

/**
 * Manual window-drag handler for chrome-less windows. Attach to any surface
 * that should move the window: only fires when the empty surface itself is
 * pressed (children keep their own clicks), and double-click maximizes.
 */
export function dragWindow(e: MouseEvent) {
  if (e.button !== 0 || e.target !== e.currentTarget) return;
  const win = getCurrentWindow();
  if (e.detail === 2) void win.toggleMaximize();
  else void win.startDragging();
}
