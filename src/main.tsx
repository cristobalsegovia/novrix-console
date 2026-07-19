import ReactDOM from "react-dom/client";
import App from "./App";
import "./styles.css";

// No StrictMode on purpose: its double-mount in dev would spawn every
// PTY/SSH session twice. TerminalView also guards against re-runs.
ReactDOM.createRoot(document.getElementById("root")!).render(<App />);
