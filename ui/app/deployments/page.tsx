"use client";

import { Badge, PageHeader, Table, Th, Td } from "@/components/ui";
import { usePoll, type Deployment } from "@/lib/api";
import { timeAgo } from "@/lib/utils";

export default function DeploymentsPage() {
  const { data: deps } = usePoll<Deployment[]>("/deployments", 3000);
  return (
    <div>
      <PageHeader title="Deployments" desc="Each deployment gets an immutable preview URL" />
      <Table>
        <thead><tr><Th>Project</Th><Th>Deployment</Th><Th>Preview URL</Th><Th>Functions</Th><Th>Created</Th></tr></thead>
        <tbody>
          {(deps ?? []).map((d) => (
            <tr key={d.id}>
              <Td className="font-medium">{d.project}</Td>
              <Td className="font-mono text-xs text-muted">{d.id}</Td>
              <Td className="font-mono text-xs"><a className="text-link hover:underline" href={`http://${d.alias}:8787/`}>{d.alias}</a></Td>
              <Td>{d.functions.length ? d.functions.map((f) => <Badge key={f} className="mr-1">{f}</Badge>) : <span className="text-muted">static</span>}</Td>
              <Td className="text-muted">{timeAgo(d.created_at_ms)}</Td>
            </tr>
          ))}
          {!deps?.length && <tr><Td className="text-muted">No deployments — run <code>fluidctl deploy examples/hello</code>.</Td></tr>}
        </tbody>
      </Table>
    </div>
  );
}
