import { useState } from "react";

/**
 * The only always-visible input in the app: one prompt bar at the foot of the
 * thread.
 *
 * It takes plain English and nothing else. There is no portal field, no
 * selector, and no mode switch — the backend resolves the destination, and the
 * session affordances live in the command palette so they cost no layout.
 */
export default function CommandBar({
  ready,
  running,
  onSubmit,
}: {
  ready: boolean;
  running: boolean;
  onSubmit: (prompt: string) => void;
}) {
  const [prompt, setPrompt] = useState("");

  function run() {
    const text = prompt.trim();
    if (!text || running) return;
    onSubmit(text);
    // Cleared on send: the thread holds the phrasing that ran, so the bar is
    // free for the next task.
    setPrompt("");
  }

  return (
    <form
      className="composer"
      onSubmit={event => {
        event.preventDefault();
        run();
      }}
    >
      <fieldset disabled={!ready || running}>
        <input
          placeholder="Type a task or ask to open a portal…"
          aria-label="Describe a task in plain English"
          value={prompt}
          onChange={event => setPrompt(event.target.value)}
        />
        <button className="primary" type="submit" disabled={!prompt.trim()}>
          {running ? "Running…" : "Run"}
        </button>
      </fieldset>
    </form>
  );
}
