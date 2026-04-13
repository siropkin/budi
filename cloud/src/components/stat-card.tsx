import { Card } from "@/components/ui/card";

export function StatCard({
  title,
  value,
  subtitle,
}: {
  title: string;
  value: string;
  subtitle?: string;
}) {
  return (
    <Card>
      <p className="text-sm font-medium text-zinc-400">{title}</p>
      <p className="mt-2 text-3xl font-semibold text-white">{value}</p>
      {subtitle && (
        <p className="mt-1 text-sm text-zinc-500">{subtitle}</p>
      )}
    </Card>
  );
}
