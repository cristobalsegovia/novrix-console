/**
 * RdpView — canvas front end for an RDP session (task 21.2).
 *
 * This component establishes the correct *structure* for rendering a remote
 * desktop and forwarding input, matching Requirements 7.5 (render the remote
 * desktop) and 7.6 (forward keyboard and pointer input):
 *
 * - A `<canvas>` is the surface the remote desktop frames paint onto.
 * - Keyboard and pointer handlers capture the user's input, encode it, and
 *   would forward it to the remote via `send_input` (Requirement 7.6).
 * - A subscription to the session's frame stream would blit each decoded frame
 *   onto the canvas (Requirement 7.5).
 *
 * ## Why this is structure-only today
 *
 * The IronRDP frame pipeline is a `TODO(wiring)` seam in the backend: `open_session`
 * returns `ipc.notYetWired` (NOT_WIRED_CODE) for RDP, so an RDP tab never
 * reaches a live `connected` state through this UI yet. In addition, there is
 * no frame event channel equivalent to `session://{id}/output` for RDP frames.
 * This view therefore:
 *
 * 1. Renders the canvas and installs the input handlers so the capture/forward
 *    path is complete and testable the moment a session goes live.
 * 2. Shows a clear overlay — "RDP not yet available (backend in integration)" —
 *    whenever the session is not actually connected, so the user is never left
 *    staring at a blank canvas.
 *
 * ## Integration points (when IronRDP emits frames)
 *
 * - FRAME STREAM: subscribe where `subscribeFrames` is marked TODO. The likely
 *   shape mirrors session output: a per-id event (e.g. `rdp://{id}/frame`)
 *   carrying a decoded bitmap region { x, y, width, height, bytes }. Blit it
 *   onto the canvas 2D context (or swap to WebGL for throughput).
 * - INPUT ENCODING: `send_input` currently carries terminal bytes. RDP input is
 *   not byte-stream shaped, so forwarding pointer/keyboard events needs either a
 *   dedicated command (e.g. `send_rdp_input`) or an agreed binary encoding over
 *   `send_input`. The handlers below build a neutral descriptor and call
 *   `forwardInput`, the single place to wire that encoding.
 *
 * Branding is task 22; styling here is neutral and token-driven.
 */

import { useEffect, useRef } from "react";
import { asDomainError, sendInput, type SessionId, type SessionState } from "../ipc";

interface RdpViewProps {
  /** The RDP session this view hosts. */
  sessionId: SessionId;
  /** Current connection state; drives the "not available" overlay. */
  state: SessionState;
  /** Reports a non-fatal error (e.g. a failed input forward) to the host. */
  onError?: (message: string) => void;
}

/** A protocol-neutral description of one input event, ready to be encoded. */
type RdpInputEvent =
  | { kind: "key"; code: string; down: boolean }
  | { kind: "pointerMove"; x: number; y: number }
  | { kind: "pointerButton"; button: number; x: number; y: number; down: boolean }
  | { kind: "wheel"; deltaX: number; deltaY: number; x: number; y: number };

function RdpView({ sessionId, state, onError }: RdpViewProps) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const onErrorRef = useRef(onError);
  onErrorRef.current = onError;

  const connected = state === "connected";

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;

    /**
     * The single seam where captured input is encoded and forwarded to the
     * remote (Requirement 7.6). Today `send_input` is byte-stream shaped (built
     * for terminals), so RDP input has no faithful encoding yet; we keep the
     * capture path intact and no-op the transmission until the RDP input
     * command / encoding lands.
     *
     * TODO(rdp input wiring): replace the no-op with the real encoding —
     * either a dedicated `send_rdp_input` command or an agreed binary frame
     * over `send_input(sessionId, encode(event))`.
     */
    const forwardInput = (event: RdpInputEvent) => {
      if (!connected) return;
      // Integration point: encode `event` and transmit. Example once wired:
      //   sendInput(sessionId, encodeRdpInput(event)).catch(reportError);
      void event;
      void sendInput;
      void sessionId;
    };

    const reportError = (err: unknown) =>
      onErrorRef.current?.(asDomainError(err).message);
    void reportError;

    // ----- Keyboard (Requirement 7.6) -----
    const handleKeyDown = (e: KeyboardEvent) => {
      e.preventDefault();
      forwardInput({ kind: "key", code: e.code, down: true });
    };
    const handleKeyUp = (e: KeyboardEvent) => {
      e.preventDefault();
      forwardInput({ kind: "key", code: e.code, down: false });
    };

    // ----- Pointer (Requirement 7.6) -----
    const relativePoint = (e: MouseEvent) => {
      const rect = canvas.getBoundingClientRect();
      return { x: e.clientX - rect.left, y: e.clientY - rect.top };
    };
    const handlePointerMove = (e: MouseEvent) => {
      const { x, y } = relativePoint(e);
      forwardInput({ kind: "pointerMove", x, y });
    };
    const handlePointerDown = (e: MouseEvent) => {
      const { x, y } = relativePoint(e);
      forwardInput({ kind: "pointerButton", button: e.button, x, y, down: true });
    };
    const handlePointerUp = (e: MouseEvent) => {
      const { x, y } = relativePoint(e);
      forwardInput({ kind: "pointerButton", button: e.button, x, y, down: false });
    };
    const handleWheel = (e: WheelEvent) => {
      e.preventDefault();
      const { x, y } = relativePoint(e);
      forwardInput({ kind: "wheel", deltaX: e.deltaX, deltaY: e.deltaY, x, y });
    };
    const handleContextMenu = (e: MouseEvent) => {
      // The remote desktop owns the right-click menu, not the browser.
      e.preventDefault();
    };

    canvas.addEventListener("keydown", handleKeyDown);
    canvas.addEventListener("keyup", handleKeyUp);
    canvas.addEventListener("mousemove", handlePointerMove);
    canvas.addEventListener("mousedown", handlePointerDown);
    canvas.addEventListener("mouseup", handlePointerUp);
    canvas.addEventListener("wheel", handleWheel, { passive: false });
    canvas.addEventListener("contextmenu", handleContextMenu);

    // ----- Frame stream (Requirement 7.5) -----
    // TODO(rdp frame wiring): subscribe to the per-id RDP frame stream once the
    // backend emits it (see module docs for the expected shape) and blit each
    // frame onto `canvas.getContext("2d")`. Shape mirrors listenSessionOutput:
    //   const unsub = await listenRdpFrames(sessionId, (frame) => blit(ctx, frame));
    const subscribeFrames = (): (() => void) | null => {
      // No frame channel yet; nothing to subscribe to.
      return null;
    };
    const unsubscribeFrames = subscribeFrames();

    return () => {
      canvas.removeEventListener("keydown", handleKeyDown);
      canvas.removeEventListener("keyup", handleKeyUp);
      canvas.removeEventListener("mousemove", handlePointerMove);
      canvas.removeEventListener("mousedown", handlePointerDown);
      canvas.removeEventListener("mouseup", handlePointerUp);
      canvas.removeEventListener("wheel", handleWheel);
      canvas.removeEventListener("contextmenu", handleContextMenu);
      unsubscribeFrames?.();
    };
  }, [sessionId, connected]);

  return (
    <div className="rdp-view">
      <canvas
        ref={canvasRef}
        className="rdp-canvas"
        // tabIndex makes the canvas focusable so it can capture key events.
        tabIndex={0}
        aria-label="Remote desktop"
      />
      {!connected ? (
        <div className="rdp-overlay" role="status">
          <p className="rdp-overlay-title">RDP not yet available</p>
          <p className="rdp-overlay-note">
            The remote desktop pipeline is still being wired to the backend.
          </p>
        </div>
      ) : null}
    </div>
  );
}

export default RdpView;
