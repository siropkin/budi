import { useMemo } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { ErrorState, LoadingState } from "@/components/state";
import {
  fetchSettings,
  fetchUpdateCheck,
  postInstallIntegrations,
  postMigrate,
  postRepair,
  postSyncAll,
  postSyncRecent,
  postSyncReset,
} from "@/lib/api";
import { fmtDate, fmtNum, fmtSyncTime, formatPath } from "@/lib/format";

const INTEGRATIONS = [
  { key: "claude_code_hooks", label: "Claude Code Hooks", component: "claude-code-hooks" },
  { key: "cursor_hooks", label: "Cursor Hooks", component: "cursor-hooks" },
  { key: "cursor_extension", label: "Cursor Extension", component: "cursor-extension" },
  { key: "mcp_server", label: "MCP Server", component: "claude-code-mcp" },
  { key: "otel", label: "OTEL", component: "claude-code-otel" },
  { key: "statusline", label: "Statusline", component: "claude-code-statusline" },
  { key: "starship", label: "Starship Prompt", component: "starship" },
] as const;

async function runFullResync() {
  await postSyncReset();
  return postSyncAll();
}

export function SettingsPage() {
  const queryClient = useQueryClient();

  const settingsQuery = useQuery({
    queryKey: ["settings"],
    queryFn: ({ signal }) => fetchSettings(signal),
    refetchInterval: 5000,
  });

  const syncRecentMutation = useMutation({
    mutationFn: postSyncRecent,
    onSuccess: (result) => {
      toast.success(`Recent sync complete (${String(result.files_synced ?? 0)} files)`);
      void queryClient.invalidateQueries({ queryKey: ["settings"] });
    },
    onError: (error: Error) => toast.error(error.message),
  });

  const fullResyncMutation = useMutation({
    mutationFn: runFullResync,
    onSuccess: (result) => {
      toast.success(`Full re-sync complete (${String(result.files_synced ?? 0)} files)`);
      void queryClient.invalidateQueries({ queryKey: ["settings"] });
    },
    onError: (error: Error) => toast.error(error.message),
  });

  const migrateMutation = useMutation({
    mutationFn: postMigrate,
    onSuccess: (result) => {
      toast.success(`Migration done (current: v${String(result.current ?? "?")})`);
      void queryClient.invalidateQueries({ queryKey: ["settings"] });
    },
    onError: (error: Error) => toast.error(error.message),
  });

  const repairMutation = useMutation({
    mutationFn: postRepair,
    onSuccess: () => {
      toast.success("Database repair completed");
      void queryClient.invalidateQueries({ queryKey: ["settings"] });
    },
    onError: (error: Error) => toast.error(error.message),
  });

  const checkUpdateMutation = useMutation({
    mutationFn: fetchUpdateCheck,
    onSuccess: (result) => {
      if (result.error) {
        toast.error(String(result.error));
        return;
      }

      if (result.up_to_date) {
        toast.success(`Already on latest version (${String(result.current)})`);
        return;
      }

      if (result.latest) {
        toast(`Update available: ${String(result.latest)} (current ${String(result.current)})`);
      } else {
        toast("Could not determine latest version");
      }
    },
    onError: (error: Error) => toast.error(error.message),
  });

  const installMutation = useMutation({
    mutationFn: postInstallIntegrations,
    onSuccess: () => {
      toast.success("Integration action completed");
      void queryClient.invalidateQueries({ queryKey: ["settings"] });
    },
    onError: (error: Error) => toast.error(error.message),
  });

  const loading =
    syncRecentMutation.isPending ||
    fullResyncMutation.isPending ||
    migrateMutation.isPending ||
    repairMutation.isPending ||
    checkUpdateMutation.isPending ||
    installMutation.isPending;

  if (settingsQuery.isPending) {
    return <LoadingState label="Loading settings..." />;
  }

  if (settingsQuery.error) {
    return <ErrorState error={settingsQuery.error} onRetry={() => settingsQuery.refetch()} />;
  }

  const settings = settingsQuery.data;
  const integrationState = settings.integrations;
  const database = integrationState.database ?? {};
  const paths = integrationState.paths ?? {};

  const hasMigration = useMemo(() => Boolean(settings.schema.needs_migration), [settings.schema]);

  const askConfirmation = (message: string): boolean => window.confirm(message);

  return (
    <div className="space-y-5">
      <div className="grid gap-4 md:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle>Status</CardTitle>
          </CardHeader>
          <CardContent className="space-y-2 text-sm">
            <p>
              Version: <span className="font-medium text-foreground">{settings.health.version || "?"}</span>
            </p>
            <p>
              Last Sync:{" "}
              <span className={settings.syncStatus.syncing ? "text-amber-300" : "text-foreground"}>
                {settings.syncStatus.syncing ? "Syncing now..." : fmtSyncTime(settings.syncStatus.last_synced)}
              </span>
            </p>
            <p>
              Schema:{" "}
              <span className={hasMigration ? "text-amber-300" : "text-foreground"}>
                v{String(settings.schema.current)}
                {hasMigration ? ` (needs v${String(settings.schema.target)})` : ""}
              </span>
            </p>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>Database</CardTitle>
          </CardHeader>
          <CardContent className="space-y-2 text-sm">
            <p>
              Size: <span className="text-foreground">{database.size_mb != null ? `${database.size_mb} MB` : "--"}</span>
            </p>
            <p>
              Records: <span className="text-foreground">{database.records != null ? fmtNum(database.records) : "--"}</span>
            </p>
            <p>
              First Record: <span className="text-foreground">{database.first_record ? fmtDate(database.first_record) : "--"}</span>
            </p>
          </CardContent>
        </Card>
      </div>

      <Card>
        <CardHeader>
          <CardTitle>Integrations</CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          {INTEGRATIONS.map((integration) => {
            const active = Boolean(integrationState[integration.key]);
            return (
              <div key={integration.key} className="flex flex-wrap items-center justify-between gap-3 rounded-md border border-border bg-background px-3 py-2">
                <div className="flex items-center gap-2">
                  <span className="text-sm font-medium">{integration.label}</span>
                  <Badge variant={active ? "success" : "warning"}>{active ? "Active" : "Not set up"}</Badge>
                </div>
                <Button
                  variant="secondary"
                  size="sm"
                  disabled={loading}
                  onClick={() =>
                    installMutation.mutate({
                      components: [integration.component],
                      statusline_preset: integration.key === "statusline" ? "coach" : undefined,
                    })
                  }
                >
                  Reinstall
                </Button>
              </div>
            );
          })}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>Operations</CardTitle>
        </CardHeader>
        <CardContent className="flex flex-wrap gap-2">
          <Button variant="secondary" disabled={loading} onClick={() => syncRecentMutation.mutate()}>
            Sync Recent Data
          </Button>
          <Button
            variant="secondary"
            disabled={loading}
            onClick={() => {
              if (askConfirmation("Reset sync state and ingest all history?")) {
                fullResyncMutation.mutate();
              }
            }}
          >
            Full Re-sync
          </Button>
          {hasMigration ? (
            <Button
              variant="secondary"
              disabled={loading}
              onClick={() => {
                if (askConfirmation("Run database migration now?")) {
                  migrateMutation.mutate();
                }
              }}
            >
              Migrate Database
            </Button>
          ) : null}
          <Button
            variant="secondary"
            disabled={loading}
            onClick={() => {
              if (askConfirmation("Run schema repair now?")) {
                repairMutation.mutate();
              }
            }}
          >
            Repair Database
          </Button>
          <Button variant="outline" disabled={loading} onClick={() => checkUpdateMutation.mutate()}>
            Check for Updates
          </Button>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>Paths</CardTitle>
        </CardHeader>
        <CardContent className="space-y-2 text-sm text-muted-foreground">
          <p title={paths.database}>Database: {formatPath(paths.database)}</p>
          <p title={paths.config}>Config: {formatPath(paths.config)}</p>
          <p title={paths.claude_settings}>Claude Settings: {formatPath(paths.claude_settings)}</p>
          <p title={paths.cursor_hooks}>Cursor Hooks: {formatPath(paths.cursor_hooks)}</p>
        </CardContent>
      </Card>
    </div>
  );
}
