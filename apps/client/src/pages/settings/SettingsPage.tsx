import { useEffect, useState } from "react";
import { errorText } from "../../services/errors";
import { getServerSettings, setServerUrl } from "../../services/settings";
import type { ServerSettingsIpc } from "../../services/settings";
import { Button, EmptyState, Field, Icon, Notice, PageHeader, Spinner } from "../../components/ui";
import { FormError } from "../../components/ui/FormError";
import "../../styles/settings.css";

const BACK = { to: "/library", label: "Back to library" } as const;

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
            <div className="page page--narrow">
                <PageHeader title="Settings" back={BACK} />
                <EmptyState icon="alert" tone="error" role="alert" title="Could not load settings">
                    <span data-testid="settings-load-error">{loadError}</span>
                </EmptyState>
            </div>
        );
    }
    if (loaded === null) {
        return (
            <div className="page page--narrow">
                <PageHeader title="Settings" back={BACK} />
                <p className="loading" role="status" aria-busy="true">
                    <Spinner />
                    Loading settings...
                </p>
            </div>
        );
    }

    // The saved address only takes effect on the next launch. Compare what
    // that launch will use with what is running, so clearing an address the
    // app is currently using is reported too.
    const restartNeeded = loaded.next_url !== loaded.active_url;

    return (
        <div className="page page--narrow">
            <PageHeader title="Settings" description="Choose how Locast connects to other people." back={BACK} />

            <form className="panel settings-panel" onSubmit={onSubmit} data-testid="settings-form">
                <div className="settings-panel__head">
                    <h2 className="panel__title">Connection</h2>
                    <p className="field__hint">
                        The server that creates rooms and introduces you to other viewers. Your media never
                        passes through it.
                    </p>
                </div>

                <div className="settings-current">
                    <span className="settings-current__label">
                        <Icon name="link" />
                        Connected to
                    </span>
                    <code className="settings-current__url" data-testid="settings-active-url">
                        {loaded.active_url}
                    </code>
                </div>

                {loaded.env_override && (
                    <div data-testid="settings-env-note">
                        <Notice kind="info" role="none">
                            The LOCAST_SIGNALING_URL environment variable is set and takes precedence over the
                            address below.
                        </Notice>
                    </div>
                )}

                <Field
                    label="Server address"
                    hint="A wss:// address. Plain ws:// is only accepted for a server on this computer. Leave it empty to use the default."
                >
                    {({ id, describedBy }) => (
                        <input
                            id={id}
                            className="input input--mono"
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
                            aria-describedby={[describedBy, error !== null ? "settings-error" : null].filter(Boolean).join(" ")}
                            data-testid="settings-url"
                        />
                    )}
                </Field>
                {error !== null && (
                    <FormError id="settings-error" testId="settings-error">{error}</FormError>
                )}

                <div role="status" aria-live="polite" className="settings-status">
                    {saved && (
                        <p className="settings-status__saved" data-testid="settings-saved">
                            <Icon name="check" />
                            Saved.
                        </p>
                    )}
                    {restartNeeded && (
                        <Notice kind="warning" role="none" testId="settings-restart">
                            Restart Locast to connect to {loaded.next_url}.
                        </Notice>
                    )}
                </div>

                <div className="form-actions">
                    <Button
                        variant="ghost"
                        disabled={saving || loaded.configured_url === null}
                        onClick={() => void save(null)}
                        data-testid="settings-reset"
                    >
                        Use the default
                    </Button>
                    <Button type="submit" variant="primary" disabled={saving}>
                        {saving && <Spinner />}
                        {saving ? "Saving..." : "Save"}
                    </Button>
                </div>
            </form>
        </div>
    );
}
