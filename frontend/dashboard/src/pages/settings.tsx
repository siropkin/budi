import type { ReactNode } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { ErrorState, LoadingState } from "@/components/state";
import {
  fetchSchemaVersion,
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

type MutationToastCtx = {
  toastId: string | number;
};

async function runFullResync() {
  await postSyncReset();
  return postSyncAll();
}

function beginOperationToast(message: string): MutationToastCtx {
  return { toastId: toast.loading(message) };
}

function finishOperationSuccess(ctx: MutationToastCtx | undefined, message: string): void {
  toast.success(message, ctx ? { id: ctx.toastId } : undefined);
}

function finishOperationError(ctx: MutationToastCtx | undefined, error: Error): void {
  toast.error(error.message, ctx ? { id: ctx.toastId } : undefined);
}

function integrationLabelFromComponent(component: string | undefined): string {
  if (!component) return "integration";
  const found = INTEGRATIONS.find((integration) => integration.component === component);
  return found?.label ?? "integration";
}

function SettingRow({
  label,
  value,
  action,
}: {
  label: string;
  value: ReactNode;
  action?: ReactNode;
}) {
  return (
    <div className="flex min-h-12 items-center justify-between gap-3 rounded-md border border-border/70 bg-background px-3 py-2">
      <p className="text-sm text-muted-foreground">
        {label}: <span className="text-foreground">{value}</span>
      </p>
      {action ? <div className="shrink-0">{action}</div> : null}
    </div>
  );
}

export function SettingsPage() {
  const queryClient = useQueryClient();

  const settingsQuery = useQuery({
    queryKey: ["settings"],
    queryFn: ({ signal }) => fetchSettings(signal),
    refetchInterval: 5000,
  });

  const refreshSettings = async () => {
    await queryClient.invalidateQueries({ queryKey: ["settings"] });
    await queryClient.refetchQueries({ queryKey: ["settings"], type: "active" });
  };

  const refreshSchemaAndSettings = async () => {
    const schema = await fetchSchemaVersion();
    queryClient.setQueryData<Awaited<ReturnType<typeof fetchSettings>>>(["settings"], (previous) =>
      previous ? { ...previous, schema } : previous,
    );
    await refreshSettings();
  };

  const syncRecentMutation = useMutation<Record<string, unknown>, Error, void, MutationToastCtx>({
    mutationFn: postSyncRecent,
    onMutate: () => beginOperationToast("Sync started..."),
    onSuccess: async (result, _variables, ctx) => {
      finishOperationSuccess(ctx, `Sync complete (${String(result.files_synced ?? 0)} files)`);
      await refreshSettings();
    },
    onError: (error, _variables, ctx) => finishOperationError(ctx, error),
  });

  const fullResyncMutation = useMutation<Record<string, unknown>, Error, void, MutationToastCtx>({
    mutationFn: runFullResync,
    onMutate: () => beginOperationToast("Full re-sync started..."),
    onSuccess: async (result, _variables, ctx) => {
      finishOperationSuccess(ctx, `Full re-sync complete (${String(result.files_synced ?? 0)} files)`);
      await refreshSettings();
    },
    onError: (error, _variables, ctx) => finishOperationError(ctx, error),
  });

  const migrateMutation = useMutation<Record<string, unknown>, Error, void, MutationToastCtx>({
    mutationFn: postMigrate,
    onMutate: () => beginOperationToast("Database migration started..."),
    onSuccess: async (result, _variables, ctx) => {
      finishOperationSuccess(ctx, `Migration done (current: v${String(result.current ?? "?")})`);
      await refreshSchemaAndSettings();
    },
    onError: (error, _variables, ctx) => finishOperationError(ctx, error),
  });

  const repairMutation = useMutation<Record<string, unknown>, Error, void, MutationToastCtx>({
    mutationFn: postRepair,
    onMutate: () => beginOperationToast("Database repair started..."),
    onSuccess: async (result, _variables, ctx) => {
      const parts: string[] = [];
      if (result.migrated) parts.push(`migrated v${String(result.from_version)} → v${String(result.to_version)}`);
      const cols = (result.added_columns as string[] | undefined) ?? [];
      const idxs = (result.added_indexes as string[] | undefined) ?? [];
      if (cols.length > 0) parts.push(`${cols.length} column${cols.length > 1 ? "s" : ""} added`);
      if (idxs.length > 0) parts.push(`${idxs.length} index${idxs.length > 1 ? "es" : ""} added`);
      const detail = parts.length > 0 ? parts.join(", ") : "no changes needed";
      finishOperationSuccess(ctx, `Database repair completed: ${detail}`);
      await refreshSchemaAndSettings();
    },
    onError: (error, _variables, ctx) => finishOperationError(ctx, error),
  });

  const checkUpdateMutation = useMutation<Record<string, unknown>, Error, void, MutationToastCtx>({
    mutationFn: fetchUpdateCheck,
    onMutate: () => beginOperationToast("Checking for updates..."),
    onSuccess: (result, _variables, ctx) => {
      const toastOptions = ctx ? { id: ctx.toastId } : undefined;
      if (result.error) {
        toast.error(String(result.error), toastOptions);
        return;
      }
      if (result.up_to_date) {
        toast.success(`Already on latest version (${String(result.current)})`, toastOptions);
        return;
      }
      if (result.latest) {
        toast.success(`Update available: ${String(result.latest)} (current ${String(result.current)})`, toastOptions);
        return;
      }
      toast("Could not determine latest version", toastOptions);
    },
    onError: (error, _variables, ctx) => finishOperationError(ctx, error),
  });

  const installMutation = useMutation<Record<string, unknown>, Error, { components: string[]; statusline_preset?: string }, MutationToastCtx>({
    mutationFn: postInstallIntegrations,
    onMutate: (variables) => beginOperationToast(`Reinstalling ${integrationLabelFromComponent(variables.components[0])}...`),
    onSuccess: async (_result, variables, ctx) => {
      finishOperationSuccess(ctx, `${integrationLabelFromComponent(variables.components[0])} reinstalled`);
      await refreshSettings();
    },
    onError: (error, _variables, ctx) => finishOperationError(ctx, error),
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
  const hasMigration = Boolean(settings.schema.needs_migration);
  const lastSync = settings.syncStatus.last_sync_completed_at ?? settings.syncStatus.last_synced;
  const newestData = settings.syncStatus.newest_data_at;

  const askConfirmation = (message: string): boolean => window.confirm(message);

  return (
    <div className="space-y-5">
      <div className="grid gap-4 md:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle>Status</CardTitle>
          </CardHeader>
          <CardContent className="space-y-3 text-sm">
            <SettingRow
              label="Version"
              value={<span className="font-medium">{settings.health.version || "?"}</span>}
              action={
                <Button variant="secondary" size="sm" disabled={loading} onClick={() => checkUpdateMutation.mutate()}>
                  Check for Updates
                </Button>
              }
            />
            <SettingRow
              label="Last Sync"
              value={
                <span className={settings.syncStatus.syncing ? "text-amber-300" : "text-foreground"}>
                  {settings.syncStatus.syncing ? "Syncing now..." : fmtSyncTime(lastSync)}
                </span>
              }
              action={
                <Button variant="secondary" size="sm" disabled={loading} onClick={() => syncRecentMutation.mutate()}>
                  Sync
                </Button>
              }
            />
            <SettingRow label="Newest Data" value={fmtSyncTime(newestData)} />
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>Database</CardTitle>
          </CardHeader>
          <CardContent className="space-y-3 text-sm">
            <SettingRow
              label="Schema"
              value={
                <span className={hasMigration ? "text-amber-300" : "text-foreground"}>
                  v{String(settings.schema.current)}
                  {hasMigration ? ` (needs v${String(settings.schema.target)})` : ""}
                </span>
              }
              action={
                hasMigration ? (
                  <Button
                    variant="secondary"
                    size="sm"
                    disabled={loading}
                    onClick={() => {
                      if (askConfirmation("Run database migration now?")) {
                        migrateMutation.mutate();
                      }
                    }}
                  >
                    Migrate DB
                  </Button>
                ) : undefined
              }
            />
            <SettingRow label="Size" value={database.size_mb != null ? `${database.size_mb} MB` : "--"} />
            <SettingRow label="Records" value={database.records != null ? fmtNum(database.records) : "--"} />
            <SettingRow label="First Record" value={database.first_record ? fmtDate(database.first_record) : "--"} />
            <SettingRow
              label="Repair"
              value="Reconcile schema drift (add missing columns/indexes)"
              action={
                <Button
                  variant="secondary"
                  size="sm"
                  disabled={loading}
                  onClick={() => {
                    if (askConfirmation("Run schema repair now?")) {
                      repairMutation.mutate();
                    }
                  }}
                >
                  Repair DB
                </Button>
              }
            />
            <details className="rounded-md border border-border bg-muted/20 px-3 py-2 text-xs text-muted-foreground">
              <summary className="cursor-pointer select-none font-medium">Advanced</summary>
              <div className="mt-3 flex flex-wrap gap-2">
                <Button
                  variant="secondary"
                  size="sm"
                  disabled={loading}
                  onClick={() => {
                    if (askConfirmation("Reset sync state and ingest all history?")) {
                      fullResyncMutation.mutate();
                    }
                  }}
                >
                  Full Re-sync
                </Button>
              </div>
            </details>
          </CardContent>
        </Card>
      </div>

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
    </div>
  );
}
