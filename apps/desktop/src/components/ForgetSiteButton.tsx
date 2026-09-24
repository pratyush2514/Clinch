/**
 * The exit path for "sync once, stay logged in": delete the site's cookies
 * from Clinch's own browser profile. The daily browser is untouched — the
 * one-way rule was never violated, so revocation only clears Clinch's copy.
 */
export default function ForgetSiteButton({
  host,
  forgetting,
  onForget,
}: {
  host: string;
  forgetting: "idle" | "busy" | "done" | "failed";
  onForget: (host: string) => void;
}) {
  if (forgetting === "done") {
    return (
      <p className="notice" aria-label={`Forgot ${host}`}>
        Forgotten — the saved session is gone. The next run will read {host} as
        signed out.
      </p>
    );
  }
  return (
    <div className="actions">
      <button
        type="button"
        className="subtle"
        disabled={forgetting === "busy"}
        onClick={() => onForget(host)}
        title={`Delete ${host}'s cookies from Clinch's browser`}
      >
        {forgetting === "busy" ? "Forgetting…" : "Forget this site"}
      </button>
      {forgetting === "failed" && (
        <p className="notice">Couldn&apos;t forget — try again.</p>
      )}
    </div>
  );
}
