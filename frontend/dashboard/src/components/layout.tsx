import { NavLink, Outlet } from "react-router-dom";
import { DashboardFiltersProvider } from "@/lib/period";
import { cn } from "@/lib/utils";

const NAV_ITEMS = [
  { to: "/", label: "Overview", end: true },
  { to: "/insights", label: "Insights" },
  { to: "/sessions", label: "Sessions" },
  { to: "/settings", label: "Settings" },
];

function ShellBody() {
  return (
    <div className="mx-auto min-h-screen w-full max-w-7xl px-4 pb-10 pt-6 md:px-6">
      <header className="mb-6 rounded-lg border bg-card px-4 py-4 md:px-6">
        <div className="flex flex-col gap-4 lg:flex-row lg:items-center lg:justify-between">
          <div className="flex flex-col gap-3 lg:flex-row lg:items-center lg:gap-5">
            <div className="flex items-center gap-4">
              <div className="flex h-10 w-10 items-center justify-center rounded-md border bg-muted text-lg">📊</div>
              <div className="space-y-1">
                <h1 className="text-xl font-semibold tracking-tight text-foreground">budi</h1>
              </div>
            </div>
            <nav className="flex flex-wrap items-center gap-2">
              {NAV_ITEMS.map((item) => (
                <NavLink
                  key={item.to}
                  to={item.to}
                  end={item.end}
                  className={({ isActive }) =>
                    cn(
                      "rounded-md px-3 py-2 text-sm font-medium transition",
                      isActive ? "bg-primary text-primary-foreground" : "text-muted-foreground hover:bg-muted hover:text-foreground",
                    )
                  }
                >
                  {item.label}
                </NavLink>
              ))}
            </nav>
          </div>
        </div>
      </header>
      <main>
        <Outlet />
      </main>
    </div>
  );
}

export function AppLayout() {
  return (
    <DashboardFiltersProvider>
      <ShellBody />
    </DashboardFiltersProvider>
  );
}
