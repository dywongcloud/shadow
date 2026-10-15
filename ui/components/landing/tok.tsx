/** A token that must not tear across two lines: a flag, an algorithm name or an id in running text. */
export function Tok({ children }: { children: React.ReactNode }) {
  return <span className="whitespace-nowrap">{children}</span>;
}
