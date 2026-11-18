/** ONE globe artwork, shared by the landing keystone and the social card. The
 *  seven strokes are transcribed verbatim from `public/globe-dark.svg`, the
 *  globe /new renders for an empty deployment state. That asset's inline
 *  `style="stroke:color(display-p3 ...)"` and `style="mask-type:alpha"` CSS are
 *  NOT portable to Satori's resvg, so here the stroke is a literal and the
 *  asset's fade is a radial gradient stroke: resvg and browsers both rasterise
 *  it, with no `<mask>` to be silently dropped. The asset's mask was measured
 *  to be an opacity ramp only (a gradient stroke differs by 0.28/255 mean).
 *  Light is the gradient falloff and a stroke stack, never a blur filter.
 */
const GLOBE_VIEWBOX = "0 0 688 256";
const GLOBE_WIDTH = 688;
const GLOBE_HEIGHT = 256;

/** Cool light-slate: the asset's #454545 is ~2.2:1 on the landing's and the
 *  share card's black. The gradient still fades the rim to 40%. */
export const GLOBE_STROKE = "#9ba4bb";

/** Nine sample nodes on the /new globe's own meridian/parallel intersections.
 *  They carry no count, duration, price or region name; they are decoration. */
const GLOBE_NODES: readonly (readonly [number, number])[] = [
  [344, 14], [215, 50], [473, 50], [168, 97], [432, 97],
  [139, 140], [520, 140], [109, 195], [461, 195],
];

/** Links between those nodes, by index. A sample topology, not a fleet map. */
const GLOBE_LINKS: readonly (readonly [number, number])[] = [
  [0, 1], [0, 2], [1, 3], [2, 4], [3, 5], [3, 7],
  [4, 6], [4, 8], [5, 7], [6, 8], [1, 4], [2, 3],
];

const GLOBE_PATHS: readonly string[] = [
  "M835.5 490.5C835.5 761.118 615.449 980.5 344 980.5C72.5506 980.5 -147.5 761.118 -147.5 490.5C-147.5 219.882 72.5506 0.5 344 0.5C615.449 0.5 835.5 219.882 835.5 490.5Z",
  "M490.936 489.956C490.936 625.218 474.437 747.656 447.774 836.262C434.442 880.569 418.576 916.387 400.981 941.118C383.377 965.862 364.115 979.411 344 979.411C323.885 979.411 304.622 965.862 287.019 941.118C269.424 916.387 253.558 880.569 240.226 836.262C213.563 747.656 197.064 625.218 197.064 489.956C197.064 354.693 213.563 232.255 240.226 143.649C253.558 99.3426 269.424 63.5239 287.019 38.793C304.622 14.0493 323.885 0.5 344 0.5C364.115 0.5 383.377 14.0493 400.981 38.793C418.576 63.5239 434.442 99.3426 447.774 143.649C474.437 232.255 490.936 354.693 490.936 489.956Z",
  "M638.372 489.956C638.372 625.178 605.385 747.574 552.078 836.149C498.765 924.733 425.183 979.411 344 979.411C262.817 979.411 189.235 924.733 135.922 836.149C82.6149 747.574 49.6276 625.178 49.6276 489.956C49.6276 354.733 82.6149 232.337 135.922 143.762C189.235 55.1784 262.817 0.5 344 0.5C425.183 0.5 498.765 55.1784 552.078 143.762C605.385 232.337 638.372 354.733 638.372 489.956Z",
  "M736.668 489.956C736.668 760.383 560.769 979.411 344.004 979.411C127.24 979.411 -48.6589 760.383 -48.6589 489.956C-48.6589 219.528 127.24 0.5 344.004 0.5C560.769 0.5 736.668 219.528 736.668 489.956Z",
  "M344 979.911L344 -3.33786e-05",
  "M50.7659 97.4468H637.234",
  "M-48.0668 195.445L736.075 195.431",
];

/** A complete, self-contained SVG. The landing keystone, the share card and
 *  /new all draw this SAME artwork (the /new empty state renders the source
 *  asset, whose seven strokes these are): nine blue node squares, twelve
 *  links and a blue-white lit horizon on the /new wireframe. `idPrefix`
 *  keeps two instances on one page from sharing a gradient id. Presentation
 *  attributes only: Satori's SVG rasteriser has no CSS cascade.
 *  `fillContainer` emits `width="100%"` with no height, so the viewBox keeps
 *  its 2.69:1 ratio even in a taller card; the caller anchors the box to the
 *  bottom, never stretches the SVG with `top` AND `bottom`. */
export function globeSvgMarkup({
  stroke = GLOBE_STROKE,
  idPrefix = "share",
  fillContainer = false,
}: {
  stroke?: string;
  idPrefix?: string;
  fillContainer?: boolean;
} = {}): string {
  const grad = `${idPrefix}-grad`;
  const limb = `${idPrefix}-limb`;
  const strokes = GLOBE_PATHS.map((d) => `<path d="${d}"/>`).join("");
  const box = fillContainer ? `width="100%"` : `width="${GLOBE_WIDTH}" height="${GLOBE_HEIGHT}"`;
  const limbDef = `<linearGradient id="${limb}" gradientUnits="userSpaceOnUse" x1="0" y1="0" x2="688" y2="0"><stop offset="0" stop-color="#2a8af6" stop-opacity="0"/><stop offset="0.18" stop-color="#4d9dff"/><stop offset="0.5" stop-color="#ffffff"/><stop offset="0.82" stop-color="#4d9dff"/><stop offset="1" stop-color="#2a8af6" stop-opacity="0"/></linearGradient>`;
  // The horizon is the outermost circle's visible arc. Stacked strokes of
  // falling width and rising opacity stand in for a glow; no blur filter.
  const horizon = `<g fill="none" stroke="url(#${limb})">${[22, 12, 6, 2.2].map((w, i) => `<path d="${GLOBE_PATHS[0]}" stroke-width="${w}" stroke-opacity="${[0.05, 0.08, 0.18, 1][i]}"/>`).join("")}</g>`;
  const links = `<g fill="none" stroke="#2a8af6" stroke-opacity="0.42" stroke-width="1.2">${GLOBE_LINKS.map(([a, b]) => `<path d="M${GLOBE_NODES[a][0]} ${GLOBE_NODES[a][1]}L${GLOBE_NODES[b][0]} ${GLOBE_NODES[b][1]}"/>`).join("")}</g>`;
  const nodes = `<g>${GLOBE_NODES.map(([x, y]) => `<g transform="translate(${x - 9} ${y - 9})"><rect width="18" height="18" rx="4" fill="#04070e" stroke="#2a8af6" stroke-width="1.6"/><rect x="5" y="5" width="8" height="8" rx="1.6" fill="none" stroke="#dbe9ff" stroke-width="1.2"/></g>`).join("")}</g>`;
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="${GLOBE_VIEWBOX}" ${box} fill="none" aria-hidden="true">
<defs><radialGradient id="${grad}" gradientUnits="userSpaceOnUse" cx="344" cy="0" r="491"><stop offset="0" stop-color="${stroke}"/><stop offset="1" stop-color="${stroke}" stop-opacity="0.4"/></radialGradient>${limbDef}</defs>
<g fill="none" stroke="url(#${grad})"><g stroke-width="5" stroke-opacity="0.1">${strokes}</g><g stroke-width="1">${strokes}</g></g>
${horizon}${links}${nodes}
</svg>`;
}
