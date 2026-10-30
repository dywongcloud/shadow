"use client";

import { useCallback, useMemo } from "react";
import { apiGet, usePoll, type Deployment } from "@/lib/api";

export type LastBuildState = "ready" | "error" | "cancelled";

export interface LastBuild {
  id: string;
  state: LastBuildState;
  started_ms: number;
  finished_ms: number;
  repo_url?: string;
  branch?: string;
  error?: string;
  deployment_id?: string | null;
}

/** One row of `GET /v1/gitops/projects?view=summary`. */
export interface ProjectSummary {
  project: string;
  team: string;
  created_ms: number;
  updated_ms: number;
  framework: string;
  root_dir: string;
  repo_url: string;
  production_branch: string;
  deployment_count: number;
  newest_deployment_ms: number;
  production_alias: string | null;
  first_deployed_ms: number;
  last_build: LastBuild | null;
  building: { id: string; started_ms: number } | null;
}

export interface ProjectEntry {
  project: string;
  /** Newest deployment, when the project has one visible. */
  deployment: Deployment | null;
  /** Inventory row, when the backend serves the summary view. */
  summary: ProjectSummary | null;
  /** Newest of deployment time, last build start, row creation — for ordering. */
  activityMs: number;
}

export const SUMMARY_PATH = "/v1/gitops/projects?view=summary";

/** Deployment-first merge: a summary row only adds a project the deployment list does not already show, and never while the backend counts a deployment this poll happened to
 *  miss (the two reads are independent). */
export function mergeProjects(deps: Deployment[] | null, rows: ProjectSummary[] | null): ProjectEntry[] {
  const byProject = new Map<string, ProjectEntry>();
  for (const d of deps ?? []) {
    if (!byProject.has(d.project)) {
      byProject.set(d.project, { project: d.project, deployment: d, summary: null, activityMs: d.created_at_ms });
    }
  }
  for (const row of rows ?? []) {
    const existing = byProject.get(row.project);
    if (existing) {
      existing.summary = row;
      existing.activityMs = Math.max(existing.activityMs, row.last_build?.started_ms ?? 0, row.building?.started_ms ?? 0);
      continue;
    }
    if (row.deployment_count > 0) continue;
    byProject.set(row.project, {
      project: row.project,
      deployment: null,
      summary: row,
      activityMs: Math.max(row.building?.started_ms ?? 0, row.last_build?.started_ms ?? 0, row.created_ms, row.updated_ms),
    });
  }
  return Array.from(byProject.values()).sort((a, b) => b.activityMs - a.activityMs);
}

/** The visible state of a project card. */
export function projectState(p: ProjectEntry): "building" | "error" | "cancelled" | "ready" | "empty" {
  if (p.summary?.building) return "building";
  if (p.deployment) return p.deployment.state === "ready" ? "ready" : p.deployment.state === "error" ? "error" : p.deployment.state === "cancelled" ? "cancelled" : "building";
  const last = p.summary?.last_build;
  if (!last) return "empty";
  return last.state === "ready" ? "ready" : last.state;
}

/** Every project name the tenant owns, for one-shot lookups such as the command bar. */
export async function loadProjectNames(): Promise<string[]> {
  const [deployments, rows] = await Promise.all([
    apiGet<Deployment[]>("/deployments").catch(() => null),
    apiGet<ProjectSummary[]>(SUMMARY_PATH).catch(() => null),
  ]);
  return mergeProjects(deployments, rows).map((p) => p.project);
}

/** Alphabetical project names for a picker, from the same merged list as every other project surface, so a project whose builds all failed is selectable. */
export function useProjectNames(intervalMs = 15000): string[] {
  const { projects } = useProjects(intervalMs);
  return useMemo(() => projects.map((p) => p.project).sort(), [projects]);
}

/** The one project list for every surface: `/deployments` alone omits a project whose builds never produced a deployment (a failed first build), which then stayed invisible while its
 *  name stayed reserved, so this merges it with the backend's inventory view (every row the tenant owns), deployment first. The summary poll is independent of the deployment poll: an
 *  older backend answers 404 and the dashboard keeps working on deployments alone. */
export function useProjects(intervalMs = 15000) {
  const deployments = usePoll<Deployment[]>("/deployments", intervalMs);
  const summary = usePoll<ProjectSummary[]>(SUMMARY_PATH, intervalMs);
  const projects = useMemo(
    () => mergeProjects(deployments.data, summary.error ? null : summary.data),
    [deployments.data, summary.data, summary.error],
  );
  const refresh = useCallback(async () => {
    await Promise.all([deployments.refresh(), summary.refresh()]);
  }, [deployments, summary]);
  return {
    projects,
    deployments: deployments.data,
    /** Loading follows the deployment list only (the summary view is additive). */
    loading: deployments.data === null,
    refresh,
  };
}
