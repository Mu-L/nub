/* The pm-shim opt-in demo block. A real captured terminal transcript that needs
   ONE muted line (`pnpm@<v> (via nub shim)` — the dim stderr notice the shim
   prints on every pinned-PM dispatch), which a plain ```console fence can't do:
   shiki's console grammar colors every output line the same, so the dim notice
   would render loud. This mirrors fumadocs' code-figure chrome exactly
   (`bg-fd-card` + border + rounded-xl, IBM Plex Mono at the same size/leading) so
   it sits among the page's other console fences indistinguishably, while rendering
   the one notice line in `text-fd-muted-foreground`. Plain text directly in the
   block — fully selectable, no `select-none`, no inline-code pill. */

// One line of the transcript. `dim` mutes it; `prompt` is a `$ ` command line.
type Line = { text: string; dim?: boolean; prompt?: boolean };

const lines: Line[] = [
  { text: 'nub pm shim', prompt: true },
  { text: 'nub pm shim: 7 entries in ~/.nub/shims (7 created)' },
  { text: '  PATH: added ~/.nub/shims to PATH (~/.zshrc) — restart your shell' },
  { text: '' },
  { text: 'which pnpm', prompt: true },
  { text: '~/.nub/shims/pnpm' },
  { text: '' },
  { text: 'pnpm --version', prompt: true },
  { text: 'Using pnpm 9.5.0' },
  { text: 'Installing... (4 MB)' },
  { text: 'Installed in 0.8s' },
  { text: 'pnpm@9.5.0 (via nub shim)', dim: true },
  { text: '9.5.0' },
];

export function ShimDemo() {
  return (
    <figure className="not-prose my-4 overflow-hidden rounded-xl border border-fd-border bg-fd-card shadow-sm">
      <pre className="overflow-x-auto px-5 py-4 font-mono text-[0.8rem] leading-[1.43] text-fd-foreground">
        {lines.map((line, i) => (
          <div key={i} className="whitespace-pre">
            {line.prompt ? (
              /* Match the page's plain ```console fences, where the `$ ` prompt is
                 ordinary selectable text — no `select-none`, no special glyph
                 color — so this block reads as just another console fence. */
              <span>{`$ ${line.text}`}</span>
            ) : (
              <span className={line.dim ? 'text-fd-muted-foreground' : undefined}>
                {/* Zero-width space keeps blank lines from collapsing. */}
                {line.text || '​'}
              </span>
            )}
          </div>
        ))}
      </pre>
    </figure>
  );
}
