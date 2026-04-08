import { Suspense, lazy } from "react";
import { Navigate, Route, Routes } from "react-router-dom";
import { Toaster } from "sonner";
import { AppLayout } from "@/components/layout";

const InsightsPage = lazy(async () => {
  const mod = await import("@/pages/insights");
  return { default: mod.InsightsPage };
});
const OverviewPage = lazy(async () => {
  const mod = await import("@/pages/overview");
  return { default: mod.OverviewPage };
});
const SessionDetailPage = lazy(async () => {
  const mod = await import("@/pages/session-detail");
  return { default: mod.SessionDetailPage };
});
const SessionsPage = lazy(async () => {
  const mod = await import("@/pages/sessions");
  return { default: mod.SessionsPage };
});
const SettingsPage = lazy(async () => {
  const mod = await import("@/pages/settings");
  return { default: mod.SettingsPage };
});

function RouteFallback() {
  return (
    <div className="flex h-[50vh] items-center justify-center text-sm text-muted-foreground">
      Loading dashboard...
    </div>
  );
}

function App() {
  return (
    <>
      <Suspense fallback={<RouteFallback />}>
        <Routes>
          <Route element={<AppLayout />}>
            <Route index element={<OverviewPage />} />
            <Route path="insights" element={<InsightsPage />} />
            <Route path="sessions" element={<SessionsPage />} />
            <Route path="sessions/:sessionId" element={<SessionDetailPage />} />
            <Route path="settings" element={<SettingsPage />} />
            <Route path="*" element={<Navigate to="/" replace />} />
          </Route>
        </Routes>
      </Suspense>
      <Toaster richColors position="top-right" />
    </>
  );
}

export default App;
