"use client";

import { useEffect, useState, use } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { Loader2 } from "lucide-react";
import { Button, Input, SettingCard, Badge } from "@/components/ui";
import { apiGet, apiSend, type Deployment, type ProjectSettings } from "@/lib/api";
import { timeAgo } from "@/lib/utils";
import { deploymentUrl, deploymentHost } from "@/lib/deploy-url";

export function GeneralSettings({ paramsPromise }: { paramsPromise: Promise<{ project: string }> }) {
  const params = use(paramsPromise);
  const project = decodeURIComponent(params.project);
  const router = useRouter();
  const [dep, setDep] = useState<Deployment | null>(null);
  const [prodBranch, setProdBranch] = useState<string>("");
  const [deleting, setDeleting] = useState(false);

  async function deleteProject() {
    if (deleting) return;
    if (!confirm(`Delete the entire "${project}" project and ALL its deployments? This cannot be undone.`)) return;
    setDeleting(true);
    try {
      await apiSend("DELETE", `/v1/projects/${encodeURIComponent(project)}`);
      router.push("/projects");
    } catch (e) {
      alert(String(e));
      setDeleting(false);
    }
  }

  useEffect(() => {
    apiGet<Deployment[]>("/deployments")
      .then((all) => setDep(all.find((d) => d.project === project) ?? null))
      .catch(() => {});
    apiGet<ProjectSettings>(`/v1/projects/${encodeURIComponent(project)}/settings`)
      .then((s) => setProdBranch(s?.production_branch || ""))
      .catch(() => {});
  }, [project]);

  return (
    <div className="flex flex-col gap-6">
      <SettingCard
        title="Project Name"
        desc="Used to identify your project across the dashboard and CLI."
        footer="A new deployment is required for changes to take effect."
        footerAction={<Button>Save</Button>}
      >
        <Input defaultValue={project} className="max-w-sm" />
      </SettingCard>

      <SettingCard title="Project ID & Source" desc="Connection details for this project.">
        <div className="flex flex-col divide-y divide-border text-sm">
          <Row label="Project" value={<span className="font-mono">{project}</span>} />
          <Row label="Production URL" value={dep ? <a className="text-link hover:underline" href={deploymentUrl(dep.alias, dep.region_code)} target="_blank" rel="noreferrer">{deploymentHost(dep.alias, dep.region_code)}</a> : "—"} />
          <Row
            label="Git"
            value={
              <Link href={`/projects/${encodeURIComponent(project)}/settings/git`} className="hover:underline">
                {dep?.git ? <span className="font-mono text-xs">{dep.git.repo_url} @ {dep.git.branch}</span> : <Badge>Not connected — connect →</Badge>}
              </Link>
            }
          />
          <Row
            label="Production Branch"
            value={prodBranch ? <span className="font-mono text-xs">{prodBranch}</span> : "—"}
          />
          <Row label="Last deployed" value={dep ? `${timeAgo(dep.created_at_ms)} ago by ${dep.creator}` : "—"} />
        </div>
      </SettingCard>

      <div className="flex flex-wrap gap-2 text-sm">
        <Link href={`/projects/${encodeURIComponent(project)}/settings/git`}><Button variant="outline">Git →</Button></Link>
        <Link href={`/projects/${encodeURIComponent(project)}/settings/environment-variables`}><Button variant="outline">Environment Variables →</Button></Link>
        <Link href={`/projects/${encodeURIComponent(project)}/settings/functions`}><Button variant="outline">Functions →</Button></Link>
        <Link href={`/projects/${encodeURIComponent(project)}/settings/build`}><Button variant="outline">Build & Development →</Button></Link>
      </div>

      <div className="rounded-xl border border-red-500/30 bg-red-500/[0.03] p-5">
        <h3 className="text-base font-semibold text-red-600 dark:text-red-400">Delete Project</h3>
        <p className="mt-1 text-sm text-secondary">Permanently remove this project and all its deployments. This cannot be undone.</p>
        <div className="mt-3">
          <Button variant="danger" onClick={deleteProject} disabled={deleting}>
            {deleting ? <Loader2 className="h-4 w-4 animate-spin" /> : null} Delete Project
          </Button>
        </div>
      </div>
    </div>
  );
}

function Row({ label, value }: { label: string; value: React.ReactNode }) {
  return (
    <div className="flex items-center justify-between py-2.5">
      <span className="text-secondary">{label}</span>
      <span className="font-medium">{value}</span>
    </div>
  );
}
