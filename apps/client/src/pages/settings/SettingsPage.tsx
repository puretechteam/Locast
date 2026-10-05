import { useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { getServerSettings, setServerUrl } from "../../services/settings";
import type { ServerSettingsIpc } from "../../services/settings";

function errorText(err: unknown): string {
    if (err instanceof Error) return err.message;
    if (typeof err === "object" && err !== null && "message" in err) {
        return String((err as { message: unknown }).message);
    }
    return String(err);
}

export function SettingsPage(): JSX.Element {
    const [loaded, setLoaded] = useState<ServerSettingsIpc | null>(null);
    const [loadError, setLoadError] = useState<string | null>(null);
    const [draft, setDraft] = useState("");
    const [saving, setSaving] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const [saved, setSaved] = useState(false);

    useEffect(() => {
        let cancelled = false;
        getServerSettings()
            .then((s) => {
                if (cancelled) return;
                setLoaded(s);
                setDraft(s.configured_url ?? "");
            })
            .catch((err: unknown) => {
                if (!cancelled) setLoadError(errorText(err));
            });
        return () => {
            cancelled = true;
        };
    }, []);

    async function save(next: string | null): Promise<void> {
        if (saving) return;
        setSaving(true);
        setError(null);
        setSaved(false);
        try {
            const s = await setServerUrl(next);
            setLoaded(s);
            setDraft(s.configured_url ?? "");
            setSaved(true);
        } catch (err) {
            setError(errorText(err));
        } finally {
            setSaving(false);
        }
    }

    function onSubmit(e: React.FormEvent): void {
        e.preventDefault();
        const value = draft.trim();
        void save(value.length > 0 ? value : null);
    }

    if (loadError !== null) {
        return (
            <div>
                <p className="form__error" role="alert" data-testid="settings-load-error">
                    {loadError}
                </p>
                <p>
                    <Link to="/library">Back to library</Link>
                </p>
            </div>
        );
    }
    if (loaded === null) {
        return (
            <p role="status" aria-busy="true">
                Loading settings...
            </p>
        );
    }

    // The saved address only takes effect on the next launch. Compare what
    // that launch will use with what is running, so clearing an address the
    // app is currently using is reported too.
    const restartNeeded = loaded.next_url !== loaded.active_url;

    return (
        <form className="form" onSubmit={onSubmit} data-testid="settings-form">
            <p>
                Connected to: <code data-testid="settings-active-url">{loaded.active_url}</code>
            </p>
            {loaded.env_override && (
                <p role="status" data-testid="settings-env-note">
                    The LOCAST_SIGNALING_URL environment variable is set and takes precedence over
                    the address below.
                </p>
            )}
            <label className="form__label">
                <span>Server address</span>
                <input
                    className="form__input"
                    type="text"
                    value={draft}
                    onChange={(e) => {
                        setDraft(e.target.value);
                        setSaved(false);
                        setError(null);
                    }}
                    placeholder="wss://locast.example.com/ws"
                    autoComplete="off"
                    spellCheck={false}
                    readOnly={saving}
                    aria-invalid={error !== null}
                    aria-describedby={error !== null ? "settings-error" : undefined}
                    data-testid="settings-url"
                />
            </label>
            {error !== null && (
                <p
                    id="settings-error"
                    className="form__error"
                    role="alert"
                    data-testid="settings-error"
                >
                    {error}
                </p>
            )}
            <div role="status" aria-live="polite">
                {saved && <p data-testid="settings-saved">Saved.</p>}
                {restartNeeded && (
                    <p data-testid="settings-restart">
                        Restart Locast to connect to {loaded.next_url}.
                    </p>
                )}
            </div>
            <button className="form__submit" type="submit" disabled={saving}>
                {saving ? "Saving..." : "Save"}
            </button>
            <button
                className="form__submit"
                type="button"
                disabled={saving || loaded.configured_url === null}
                onClick={() => void save(null)}
                data-testid="settings-reset"
            >
                Use the default
            </button>
            <p>
                <Link to="/library">Back to library</Link>
            </p>
        </form>
    );
}
