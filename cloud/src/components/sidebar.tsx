"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { clsx } from "clsx";
import {
  BarChart3,
  GitBranch,
  LayoutDashboard,
  Cpu,
  Settings,
  Timer,
  Users,
} from "lucide-react";

const NAV_ITEMS = [
  { href: "/dashboard", label: "Overview", icon: LayoutDashboard },
  { href: "/dashboard/team", label: "Team", icon: Users },
  { href: "/dashboard/models", label: "Models", icon: Cpu },
  { href: "/dashboard/repos", label: "Repos", icon: GitBranch },
  { href: "/dashboard/sessions", label: "Sessions", icon: Timer },
  { href: "/dashboard/settings", label: "Settings", icon: Settings },
] as const;

export function Sidebar() {
  const pathname = usePathname();

  return (
    <nav className="flex w-56 flex-col border-r border-white/10 bg-[#0a0a0a] px-3 py-4">
      <Link
        href="/dashboard"
        className="mb-6 flex items-center gap-2 px-3 text-lg font-bold text-white"
      >
        <BarChart3 className="h-5 w-5 text-blue-500" />
        Budi Cloud
      </Link>
      <ul className="space-y-1">
        {NAV_ITEMS.map((item) => {
          const isActive =
            item.href === "/dashboard"
              ? pathname === "/dashboard"
              : pathname.startsWith(item.href);
          const Icon = item.icon;
          return (
            <li key={item.href}>
              <Link
                href={item.href}
                className={clsx(
                  "flex items-center gap-2.5 rounded-lg px-3 py-2 text-sm font-medium transition-colors",
                  isActive
                    ? "bg-white/10 text-white"
                    : "text-zinc-400 hover:bg-white/5 hover:text-zinc-200"
                )}
              >
                <Icon className="h-4 w-4" />
                {item.label}
              </Link>
            </li>
          );
        })}
      </ul>
    </nav>
  );
}
