import { AlertTriangle } from "lucide-react";
import { Button } from "@/components/ui/button";

export function LoadingState({ label = "Loading analytics..." }: { label?: string }) {
  return (
    <div className="flex h-48 items-center justify-center rounded-xl border border-border bg-card text-muted-foreground">
      {label}
    </div>
  );
}

export function EmptyState({ label }: { label: string }) {
  return (
    <div className="flex h-32 items-center justify-center rounded-xl border border-dashed border-border text-sm text-muted-foreground">
      {label}
    </div>
  );
}

export function ErrorState({
  error,
  onRetry,
}: {
  error: Error;
  onRetry?: () => void;
}) {
  return (
    <div className="flex flex-col items-center justify-center gap-3 rounded-xl border border-destructive/30 bg-destructive/5 p-8 text-center">
      <AlertTriangle className="h-6 w-6 text-destructive" />
      <p className="font-medium">Failed to load dashboard data</p>
      <p className="max-w-2xl text-sm text-muted-foreground">{error.message}</p>
      {onRetry ? (
        <Button variant="outline" onClick={onRetry}>
          Retry
        </Button>
      ) : null}
    </div>
  );
}
