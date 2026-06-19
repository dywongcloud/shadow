import type { Config } from "tailwindcss";

const hsl = (v: string) => `hsl(var(${v}) / <alpha-value>)`;

const config: Config = {
  darkMode: "class",
  content: [
    "./app/**/*.{ts,tsx}",
    "./components/**/*.{ts,tsx}",
    "./node_modules/@tremor/**/*.{js,ts,jsx,tsx}",
  ],
  theme: {
    extend: {
      colors: {
        bg: hsl("--background"),
        subtle: hsl("--subtle"),
        panel: hsl("--card"),
        card: hsl("--card"),
        border: hsl("--border"),
        "border-strong": hsl("--border-strong"),
        fg: hsl("--foreground"),
        secondary: hsl("--secondary"),
        muted: hsl("--muted"),
        link: hsl("--link"),
        accent: hsl("--accent"),
        "accent-fg": hsl("--accent-foreground"),
        green: hsl("--green"),
        tremor: {
          brand: { DEFAULT: hsl("--foreground"), emphasis: hsl("--foreground") },
          background: { DEFAULT: hsl("--card"), muted: hsl("--subtle"), subtle: hsl("--subtle") },
          border: { DEFAULT: hsl("--border") },
          content: { DEFAULT: hsl("--secondary"), emphasis: hsl("--foreground"), strong: hsl("--foreground") },
        },
      },
      fontFamily: {
        // ABC Diatype is the platform typeface; Geist is the graceful fallback
        // until the licensed woff2 files are added to /public/fonts.
        sans: ["ABC Diatype", "var(--font-geist-sans)", "ui-sans-serif", "system-ui", "sans-serif"],
        mono: ["ABC Diatype Mono", "var(--font-geist-mono)", "ui-monospace", "SFMono-Regular", "Menlo", "monospace"],
        // Display typeface for the Shadow brand wordmark + landing headlines.
        display: ["var(--font-display)", "var(--font-geist-sans)", "ui-sans-serif", "system-ui", "sans-serif"],
      },
      boxShadow: {
        card: "0 1px 2px rgba(0,0,0,0.04), 0 1px 1px rgba(0,0,0,0.03)",
        pop: "0 8px 30px rgba(0,0,0,0.12)",
      },
    },
  },
  safelist: [
    { pattern: /(bg|text|border|ring|fill|stroke)-(tremor)-.*/ },
    // Tremor chart colors are applied at runtime, so keep these utilities.
    {
      pattern:
        /(bg|fill|stroke|text)-(blue|amber|emerald|rose|purple|cyan|slate|green|red)-(400|500|600)/,
    },
  ],
  plugins: [],
};

export default config;
