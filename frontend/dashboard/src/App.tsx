import { Navigate, Route, Routes } from "react-router-dom";
import { Toaster } from "sonner";
import { AppLayout } from "@/components/layout";
import { InsightsPage } from "@/pages/insights";
import { OverviewPage } from "@/pages/overview";
import { SessionDetailPage } from "@/pages/session-detail";
import { SessionsPage } from "@/pages/sessions";
import { SettingsPage } from "@/pages/settings";

function App() {
  return (
    <>
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
      <Toaster richColors position="top-right" />
    </>
  );
}

export default App;
