/** The dome's light: a dark body and a lit limb drawn on the globe artwork's own silhouette. `LIMB` is that silhouette, traced from `public/globe-wireframe.png`
 *  (the outermost visible pixel along rays from the dome's centre), so the light hugs the arc instead of fogging the sky inside it. Strokes of falling width and
 *  rising opacity stand in for a blur filter, and an in-SVG mask fades everything out with the artwork's own bottom fade. 200 units of headroom above the
 *  artwork let the halo clear the apex. */
const LIMB = "M25 715.7C25.7 710.2 27.7 693.2 29.3 682.5C30.9 671.9 32.5 662 34.5 651.8C36.6 641.6 39.1 631.5 41.7 621.5C44.2 611.4 47 601.4 49.7 591.5C52.4 581.5 54.6 571.4 57.7 561.6C60.9 551.8 64.9 542.2 68.6 532.5C72.2 522.9 75.9 513.3 79.8 503.8C83.7 494.3 87.7 484.9 91.9 475.6C96.1 466.2 100.2 456.9 104.8 447.8C109.4 438.7 114.5 429.8 119.5 421C124.5 412.1 129.8 403.4 135 394.7C140.2 386 145.1 377.2 150.7 368.8C156.3 360.4 162.6 352.4 168.6 344.2C174.6 336.1 180.5 328 186.7 320.2C192.9 312.3 199.4 304.6 205.9 297C212.4 289.4 218.9 281.9 225.7 274.7C232.5 267.4 239.5 260.3 246.5 253.3C253.6 246.4 260.7 239.5 267.9 232.8C275.1 226.1 282.3 219.5 289.8 213.1C297.2 206.8 304.9 200.6 312.6 194.6C320.2 188.6 327.9 182.6 335.8 177C343.6 171.3 351.7 165.9 359.7 160.5C367.7 155.1 375.7 149.8 383.7 144.7C391.8 139.5 399.7 134.3 408 129.7C416.3 125 425.2 121 433.6 116.7C442.1 112.5 450.3 108 458.8 104C467.3 100 476 96.3 484.5 92.6C493.1 88.8 501.4 85 510.1 81.7C518.7 78.4 527.7 75.6 536.4 72.6C545.1 69.5 553.5 66.3 562.2 63.6C570.8 60.8 579.7 58.5 588.3 55.9C596.9 53.3 605.3 50.4 613.8 47.9C622.3 45.4 630.8 43 639.3 40.8C647.8 38.5 656.3 36.4 664.8 34.5C673.4 32.7 682.1 31.2 690.6 29.6C699.1 28 707.5 26.2 715.9 24.7C724.4 23.1 732.9 21.9 741.3 20.6C749.7 19.3 758 17.9 766.4 16.8C774.7 15.7 783.2 14.9 791.5 14C799.8 13 808 11.8 816.2 10.9C824.4 10.1 832.7 9.4 840.9 8.7C849.1 8 857.3 7.5 865.4 6.8C873.6 6.1 881.6 5.3 889.7 4.7C897.8 4 905.9 3.3 913.9 2.9C922 2.4 930.1 2.1 938.1 1.8C946.2 1.5 954.2 1.1 962.2 1C970.2 0.8 978.3 1.1 986.3 0.9C994.3 0.7 1002.3 -0.2 1010.3 -0.4C1018.3 -0.6 1026.3 -0.6 1034.3 -0.4C1042.3 -0.2 1050.3 0.6 1058.3 0.8C1066.3 1 1074.3 0.5 1082.4 0.7C1090.4 0.9 1098.4 1.5 1106.4 1.9C1114.5 2.3 1122.5 2.4 1130.6 2.9C1138.7 3.3 1146.7 4.1 1154.8 4.6C1162.9 5.1 1171.2 4.9 1179.3 5.6C1187.4 6.3 1195.3 8 1203.5 8.9C1211.6 9.8 1219.9 10.1 1228.2 11C1236.4 11.8 1244.6 12.9 1252.9 13.9C1261.2 14.8 1269.6 15.5 1277.9 16.6C1286.3 17.8 1294.5 19.4 1302.9 20.7C1311.2 22.1 1319.7 23.2 1328.2 24.7C1336.6 26.1 1344.9 28 1353.5 29.5C1362 31 1370.9 31.9 1379.5 33.8C1388 35.7 1396 38.7 1404.5 40.8C1413.1 42.9 1422.1 44.3 1430.8 46.5C1439.4 48.7 1448 51.3 1456.5 54C1465 56.7 1473.2 60 1481.8 62.8C1490.5 65.6 1499.5 67.8 1508.2 70.8C1516.9 73.8 1525.3 77.3 1533.9 80.6C1542.6 83.9 1551.6 86.9 1560.1 90.6C1568.7 94.2 1576.8 98.6 1585.2 102.6C1593.7 106.7 1602.2 110.7 1610.8 114.9C1619.3 119 1628.1 123.1 1636.4 127.7C1644.8 132.3 1652.8 137.4 1660.8 142.5C1668.8 147.7 1676.5 153.2 1684.4 158.6C1692.4 164 1700.6 169.2 1708.5 174.9C1716.3 180.6 1723.8 186.6 1731.3 192.7C1738.8 198.9 1745.9 205.4 1753.3 211.7C1760.8 218 1768.9 224 1776.1 230.6C1783.3 237.3 1789.8 244.5 1796.7 251.5C1803.5 258.6 1810.3 265.8 1817.1 273C1824 280.1 1831.2 287.2 1837.8 294.6C1844.5 302.1 1850.9 309.8 1857.1 317.6C1863.3 325.4 1869.4 333.4 1875.3 341.6C1881.1 349.7 1886.7 358.1 1892.3 366.4C1897.9 374.8 1903.7 383.1 1909 391.7C1914.4 400.3 1919.1 409.2 1924.1 418C1929.2 426.8 1934.7 435.5 1939.4 444.6C1944.1 453.6 1948.3 462.9 1952.4 472.2C1956.5 481.5 1960.1 491 1964.1 500.5C1968.1 509.9 1972.7 519.2 1976.4 528.8C1980.1 538.4 1983.3 548.1 1986.4 557.9C1989.5 567.7 1992.2 577.7 1995 587.6C1997.8 597.5 2000.6 607.5 2003.2 617.5C2005.8 627.5 2008.3 637.6 2010.4 647.7C2012.6 657.8 2014.5 668.4 2016.3 678.3C2018 688.1 2020.2 702.1 2021 706.9";
const BODY = `${LIMB}L2021 762L25 762Z`;

export function DomeLight() {
  return (
    <svg
      viewBox="0 -200 2048 962"
      preserveAspectRatio="xMidYMax meet"
      className="pointer-events-none absolute left-0 w-full overflow-visible"
      style={{ bottom: 0, height: "calc(100% * 962 / 762)" }}
      aria-hidden="true"
    >
      <defs>
        <linearGradient id="lp-limb" gradientUnits="userSpaceOnUse" x1="0" y1="0" x2="2048" y2="0">
          <stop offset="0" stopColor="#8b5cf6" stopOpacity="0.55" />
          <stop offset="0.3" stopColor="#60a5fa" />
          <stop offset="0.5" stopColor="#e0f2fe" />
          <stop offset="0.7" stopColor="#38bdf8" />
          <stop offset="1" stopColor="#22d3ee" stopOpacity="0.55" />
        </linearGradient>
        <linearGradient id="lp-body" gradientUnits="userSpaceOnUse" x1="0" y1="0" x2="0" y2="762">
          <stop offset="0" stopColor="#070911" stopOpacity="0.96" />
          <stop offset="0.6" stopColor="#090b12" stopOpacity="0.9" />
          <stop offset="1" stopColor="#0c0d10" stopOpacity="0.7" />
        </linearGradient>
        <linearGradient id="lp-dome-fade" gradientUnits="userSpaceOnUse" x1="0" y1="-200" x2="0" y2="762">
          <stop offset="0" stopColor="#fff" />
          <stop offset="0.652" stopColor="#fff" />
          <stop offset="0.992" stopColor="#000" />
        </linearGradient>
        <mask id="lp-dome-mask" maskUnits="userSpaceOnUse" x="-100" y="-200" width="2248" height="962">
          <rect x="-100" y="-200" width="2248" height="962" fill="url(#lp-dome-fade)" />
        </mask>
      </defs>
      <g mask="url(#lp-dome-mask)">
        <path d={BODY} fill="url(#lp-body)" />
        <g fill="none" stroke="url(#lp-limb)" strokeLinecap="round" strokeLinejoin="round">
          <path d={LIMB} strokeWidth="104" strokeOpacity="0.018" />
          <path d={LIMB} strokeWidth="78" strokeOpacity="0.026" />
          <path d={LIMB} strokeWidth="56" strokeOpacity="0.036" />
          <path d={LIMB} strokeWidth="38" strokeOpacity="0.05" />
          <path d={LIMB} strokeWidth="24" strokeOpacity="0.075" />
          <path d={LIMB} strokeWidth="14" strokeOpacity="0.12" />
          <path d={LIMB} strokeWidth="8" strokeOpacity="0.22" />
          <path d={LIMB} strokeWidth="3" strokeOpacity="0.95" />
        </g>
      </g>
    </svg>
  );
}
