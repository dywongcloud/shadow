/** The hero globe's animated tracer overlay: 15 routes, 30 SMIL sweeps. Inlined (not served as a file) so the SMIL timeline is part of the live document, and a
 *  build-time constant from this repo, never user input, so injecting it with `dangerouslySetInnerHTML` is safe. */
export const GLOBE_TRACERS_SVG = `<svg xmlns="http://www.w3.org/2000/svg"
     xmlns:xlink="http://www.w3.org/1999/xlink"
     width="2048" height="762" viewBox="0 0 2048 762"
     preserveAspectRatio="xMidYMid meet"
     fill="none" style="background:transparent" shape-rendering="geometricPrecision">
  <title>Pixel-aligned animated globe wireframe</title>
  <desc>The original transparent globe artwork is preserved unchanged. Blue luminous sweep segments travel directly along the exact blue globe routes.</desc>
  <defs>
    <filter id="wideGlow" x="-30%" y="-30%" width="160%" height="160%" color-interpolation-filters="sRGB">
      <feGaussianBlur stdDeviation="8" result="blur"/>
      <feMerge>
        <feMergeNode in="blur"/>
        <feMergeNode in="SourceGraphic"/>
      </feMerge>
    </filter>
    <filter id="tightGlow" x="-30%" y="-30%" width="160%" height="160%" color-interpolation-filters="sRGB">
      <feGaussianBlur stdDeviation="3.2" result="blur"/>
      <feMerge>
        <feMergeNode in="blur"/>
        <feMergeNode in="SourceGraphic"/>
      </feMerge>
    </filter>
    <mask id="keepSweepsBehindNodes" maskUnits="userSpaceOnUse" x="0" y="0" width="2048" height="762">
      <rect x="0" y="0" width="2048" height="762" fill="white"/>
      <rect x="183" y="604" width="81" height="81" rx="12" fill="black"/>
      <rect x="317" y="246" width="83" height="80" rx="12" fill="black"/>
      <rect x="727" y="246" width="82" height="80" rx="12" fill="black"/>
      <rect x="980" y="51" width="81" height="81" rx="12" fill="black"/>
      <rect x="1446" y="90" width="80" height="81" rx="12" fill="black"/>
      <rect x="1723" y="421" width="83" height="83" rx="12" fill="black"/>
      <rect x="980" y="604" width="81" height="80" rx="12" fill="black"/>
    </mask>
  </defs>
  <!-- Exact transparent artwork from the approved globe image. -->
  />
  <!-- Every animated path below was traced from the centerline of its matching blue route. -->
  <g id="aligned-blue-sweeps" stroke-linecap="round" stroke-linejoin="round" pointer-events="none" mask="url(#keepSweepsBehindNodes)">
    <g id="upperLeft-sweep" class="sweep-route">
      <path d="M 401.23 236.00 L 404.48 232.00 L 407.76 228.00 L 411.07 224.00 L 414.40 220.00 L 417.77 216.00 L 421.17 212.00 L 424.62 208.00 L 428.11 204.00 L 431.64 200.00 L 435.23 196.00 L 438.88 192.00 L 442.59 188.00 L 446.35 184.00 L 450.19 180.00 L 454.09 176.00 L 458.07 172.00 L 462.13 168.00 L 466.26 164.00 L 470.48 160.00 L 474.79 156.00 L 479.20 152.00 L 483.70 148.00 L 488.29 144.00 L 493.00 140.00 L 497.82 136.00 L 502.77 132.00 L 507.85 128.00 L 513.08 124.00 L 518.45 120.00 L 523.99 116.00 L 529.69 112.00 L 535.57 108.00 L 541.64 104.00 L 547.90 100.00 L 554.36 96.00 L 561.03 92.00 L 567.93 88.00 L 575.05 84.00 L 582.40 80.00 L 590.01 76.00 L 597.87 72.00 L 605.99 68.00 L 614.38 64.00 L 623.05 60.00" pathLength="1000"
            stroke="#006BFF" stroke-width="30" opacity="0"
            stroke-dasharray="307.5 692.5" stroke-dashoffset="0"
            filter="url(#wideGlow)">
        <animate attributeName="stroke-dashoffset" values="0;-1000" dur="2.90s" begin="-0.35s" repeatCount="indefinite"/>
        <animate attributeName="opacity" values="0;0.24;0.24;0" keyTimes="0;0.08;0.88;1" dur="2.90s" begin="-0.35s" repeatCount="indefinite"/>
      </path>
      <path d="M 401.23 236.00 L 404.48 232.00 L 407.76 228.00 L 411.07 224.00 L 414.40 220.00 L 417.77 216.00 L 421.17 212.00 L 424.62 208.00 L 428.11 204.00 L 431.64 200.00 L 435.23 196.00 L 438.88 192.00 L 442.59 188.00 L 446.35 184.00 L 450.19 180.00 L 454.09 176.00 L 458.07 172.00 L 462.13 168.00 L 466.26 164.00 L 470.48 160.00 L 474.79 156.00 L 479.20 152.00 L 483.70 148.00 L 488.29 144.00 L 493.00 140.00 L 497.82 136.00 L 502.77 132.00 L 507.85 128.00 L 513.08 124.00 L 518.45 120.00 L 523.99 116.00 L 529.69 112.00 L 535.57 108.00 L 541.64 104.00 L 547.90 100.00 L 554.36 96.00 L 561.03 92.00 L 567.93 88.00 L 575.05 84.00 L 582.40 80.00 L 590.01 76.00 L 597.87 72.00 L 605.99 68.00 L 614.38 64.00 L 623.05 60.00" pathLength="1000"
            stroke="#1684FF" stroke-width="9" opacity="0"
            stroke-dasharray="232.5 767.5" stroke-dashoffset="0"
            filter="url(#tightGlow)">
        <animate attributeName="stroke-dashoffset" values="0;-1000" dur="2.90s" begin="-0.35s" repeatCount="indefinite"/>
        <animate attributeName="opacity" values="0;0.98;0.98;0" keyTimes="0;0.09;0.86;1" dur="2.90s" begin="-0.35s" repeatCount="indefinite"/>
      </path>
      <path d="M 401.23 236.00 L 404.48 232.00 L 407.76 228.00 L 411.07 224.00 L 414.40 220.00 L 417.77 216.00 L 421.17 212.00 L 424.62 208.00 L 428.11 204.00 L 431.64 200.00 L 435.23 196.00 L 438.88 192.00 L 442.59 188.00 L 446.35 184.00 L 450.19 180.00 L 454.09 176.00 L 458.07 172.00 L 462.13 168.00 L 466.26 164.00 L 470.48 160.00 L 474.79 156.00 L 479.20 152.00 L 483.70 148.00 L 488.29 144.00 L 493.00 140.00 L 497.82 136.00 L 502.77 132.00 L 507.85 128.00 L 513.08 124.00 L 518.45 120.00 L 523.99 116.00 L 529.69 112.00 L 535.57 108.00 L 541.64 104.00 L 547.90 100.00 L 554.36 96.00 L 561.03 92.00 L 567.93 88.00 L 575.05 84.00 L 582.40 80.00 L 590.01 76.00 L 597.87 72.00 L 605.99 68.00 L 614.38 64.00 L 623.05 60.00" pathLength="1000"
            stroke="#C8EEFF" stroke-width="2.4" opacity="0"
            stroke-dasharray="105 895" stroke-dashoffset="0">
        <animate attributeName="stroke-dashoffset" values="0;-1000" dur="2.90s" begin="-0.35s" repeatCount="indefinite"/>
        <animate attributeName="opacity" values="0;1;1;0" keyTimes="0;0.11;0.82;1" dur="2.90s" begin="-0.35s" repeatCount="indefinite"/>
      </path>
    </g>
    <g id="lowerLeft-sweep" class="sweep-route">
      <path d="M 259.00 644.00 L 461.00 644.00 L 459.36 638.00 L 460.00 634.00 L 460.64 630.00 L 461.29 626.00 L 461.94 622.00 L 462.60 618.00 L 463.26 614.00 L 463.92 610.00 L 464.59 606.00 L 465.27 602.00 L 465.95 598.00 L 466.63 594.00 L 467.33 590.00 L 468.03 586.00 L 468.73 582.00 L 469.44 578.00 L 470.16 574.00 L 470.89 570.00 L 471.62 566.00 L 472.37 562.00 L 473.11 558.00 L 473.87 554.00 L 474.64 550.00 L 475.41 546.00 L 476.20 542.00 L 476.99 538.00 L 477.79 534.00 L 478.61 530.00 L 479.43 526.00 L 480.26 522.00 L 481.10 518.00 L 481.96 514.00 L 482.82 510.00 L 483.70 506.00 L 484.58 502.00 L 485.48 498.00 L 486.39 494.00 L 487.31 490.00 L 488.25 486.00 L 489.19 482.00 L 490.15 478.00 L 491.13 474.00 L 492.11 470.00 L 493.11 466.00 L 494.13 462.00 L 495.15 458.00 L 496.20 454.00 L 497.25 450.00 L 498.33 446.00 L 499.41 442.00 L 500.51 438.00 L 501.63 434.00 L 502.76 430.00 L 503.91 426.00 L 505.08 422.00 L 506.26 418.00 L 507.46 414.00 L 508.67 410.00 L 509.91 406.00 L 511.16 402.00 L 512.42 398.00 L 513.71 394.00 L 515.01 390.00 L 516.34 386.00 L 517.68 382.00 L 519.04 378.00 L 520.42 374.00 L 521.82 370.00 L 523.23 366.00 L 524.67 362.00 L 526.13 358.00 L 527.61 354.00 L 529.11 350.00 L 530.63 346.00 L 532.17 342.00 L 533.73 338.00 L 535.32 334.00 L 536.92 330.00 L 538.55 326.00 L 540.20 322.00 L 541.88 318.00 L 543.57 314.00 L 545.29 310.00 L 547.03 306.00 L 548.80 302.00 L 550.59 298.00 L 552.40 294.00 L 554.24 290.00 L 556.11 286.00 L 557.99 282.00 L 559.91 278.00 L 561.84 274.00 L 563.81 270.00" pathLength="1000"
            stroke="#006BFF" stroke-width="30" opacity="0"
            stroke-dasharray="205 795" stroke-dashoffset="0"
            filter="url(#wideGlow)">
        <animate attributeName="stroke-dashoffset" values="0;-1000" dur="3.15s" begin="-1.05s" repeatCount="indefinite"/>
        <animate attributeName="opacity" values="0;0.24;0.24;0" keyTimes="0;0.08;0.88;1" dur="3.15s" begin="-1.05s" repeatCount="indefinite"/>
      </path>
      <path d="M 259.00 644.00 L 461.00 644.00 L 459.36 638.00 L 460.00 634.00 L 460.64 630.00 L 461.29 626.00 L 461.94 622.00 L 462.60 618.00 L 463.26 614.00 L 463.92 610.00 L 464.59 606.00 L 465.27 602.00 L 465.95 598.00 L 466.63 594.00 L 467.33 590.00 L 468.03 586.00 L 468.73 582.00 L 469.44 578.00 L 470.16 574.00 L 470.89 570.00 L 471.62 566.00 L 472.37 562.00 L 473.11 558.00 L 473.87 554.00 L 474.64 550.00 L 475.41 546.00 L 476.20 542.00 L 476.99 538.00 L 477.79 534.00 L 478.61 530.00 L 479.43 526.00 L 480.26 522.00 L 481.10 518.00 L 481.96 514.00 L 482.82 510.00 L 483.70 506.00 L 484.58 502.00 L 485.48 498.00 L 486.39 494.00 L 487.31 490.00 L 488.25 486.00 L 489.19 482.00 L 490.15 478.00 L 491.13 474.00 L 492.11 470.00 L 493.11 466.00 L 494.13 462.00 L 495.15 458.00 L 496.20 454.00 L 497.25 450.00 L 498.33 446.00 L 499.41 442.00 L 500.51 438.00 L 501.63 434.00 L 502.76 430.00 L 503.91 426.00 L 505.08 422.00 L 506.26 418.00 L 507.46 414.00 L 508.67 410.00 L 509.91 406.00 L 511.16 402.00 L 512.42 398.00 L 513.71 394.00 L 515.01 390.00 L 516.34 386.00 L 517.68 382.00 L 519.04 378.00 L 520.42 374.00 L 521.82 370.00 L 523.23 366.00 L 524.67 362.00 L 526.13 358.00 L 527.61 354.00 L 529.11 350.00 L 530.63 346.00 L 532.17 342.00 L 533.73 338.00 L 535.32 334.00 L 536.92 330.00 L 538.55 326.00 L 540.20 322.00 L 541.88 318.00 L 543.57 314.00 L 545.29 310.00 L 547.03 306.00 L 548.80 302.00 L 550.59 298.00 L 552.40 294.00 L 554.24 290.00 L 556.11 286.00 L 557.99 282.00 L 559.91 278.00 L 561.84 274.00 L 563.81 270.00" pathLength="1000"
            stroke="#1684FF" stroke-width="9" opacity="0"
            stroke-dasharray="155 845" stroke-dashoffset="0"
            filter="url(#tightGlow)">
        <animate attributeName="stroke-dashoffset" values="0;-1000" dur="3.15s" begin="-1.05s" repeatCount="indefinite"/>
        <animate attributeName="opacity" values="0;0.98;0.98;0" keyTimes="0;0.09;0.86;1" dur="3.15s" begin="-1.05s" repeatCount="indefinite"/>
      </path>
      <path d="M 259.00 644.00 L 461.00 644.00 L 459.36 638.00 L 460.00 634.00 L 460.64 630.00 L 461.29 626.00 L 461.94 622.00 L 462.60 618.00 L 463.26 614.00 L 463.92 610.00 L 464.59 606.00 L 465.27 602.00 L 465.95 598.00 L 466.63 594.00 L 467.33 590.00 L 468.03 586.00 L 468.73 582.00 L 469.44 578.00 L 470.16 574.00 L 470.89 570.00 L 471.62 566.00 L 472.37 562.00 L 473.11 558.00 L 473.87 554.00 L 474.64 550.00 L 475.41 546.00 L 476.20 542.00 L 476.99 538.00 L 477.79 534.00 L 478.61 530.00 L 479.43 526.00 L 480.26 522.00 L 481.10 518.00 L 481.96 514.00 L 482.82 510.00 L 483.70 506.00 L 484.58 502.00 L 485.48 498.00 L 486.39 494.00 L 487.31 490.00 L 488.25 486.00 L 489.19 482.00 L 490.15 478.00 L 491.13 474.00 L 492.11 470.00 L 493.11 466.00 L 494.13 462.00 L 495.15 458.00 L 496.20 454.00 L 497.25 450.00 L 498.33 446.00 L 499.41 442.00 L 500.51 438.00 L 501.63 434.00 L 502.76 430.00 L 503.91 426.00 L 505.08 422.00 L 506.26 418.00 L 507.46 414.00 L 508.67 410.00 L 509.91 406.00 L 511.16 402.00 L 512.42 398.00 L 513.71 394.00 L 515.01 390.00 L 516.34 386.00 L 517.68 382.00 L 519.04 378.00 L 520.42 374.00 L 521.82 370.00 L 523.23 366.00 L 524.67 362.00 L 526.13 358.00 L 527.61 354.00 L 529.11 350.00 L 530.63 346.00 L 532.17 342.00 L 533.73 338.00 L 535.32 334.00 L 536.92 330.00 L 538.55 326.00 L 540.20 322.00 L 541.88 318.00 L 543.57 314.00 L 545.29 310.00 L 547.03 306.00 L 548.80 302.00 L 550.59 298.00 L 552.40 294.00 L 554.24 290.00 L 556.11 286.00 L 557.99 282.00 L 559.91 278.00 L 561.84 274.00 L 563.81 270.00" pathLength="1000"
            stroke="#C8EEFF" stroke-width="2.4" opacity="0"
            stroke-dasharray="70 930" stroke-dashoffset="0">
        <animate attributeName="stroke-dashoffset" values="0;-1000" dur="3.15s" begin="-1.05s" repeatCount="indefinite"/>
        <animate attributeName="opacity" values="0;1;1;0" keyTimes="0;0.11;0.82;1" dur="3.15s" begin="-1.05s" repeatCount="indefinite"/>
      </path>
    </g>
    <g id="centerLeft-sweep" class="sweep-route">
      <path d="M 782.52 244.00 L 783.30 240.00 L 784.11 236.00 L 784.95 232.00 L 785.81 228.00 L 786.71 224.00 L 787.63 220.00 L 788.59 216.00 L 789.57 212.00 L 790.58 208.00 L 791.62 204.00 L 792.69 200.00 L 793.78 196.00 L 794.91 192.00 L 796.06 188.00 L 797.25 184.00 L 798.46 180.00 L 799.70 176.00 L 800.97 172.00 L 802.26 168.00 L 803.59 164.00 L 804.94 160.00 L 806.32 156.00 L 807.74 152.00 L 809.17 148.00 L 810.64 144.00 L 812.14 140.00 L 813.66 136.00 L 815.21 132.00 L 816.80 128.00 L 818.42 124.00 L 820.08 120.00 L 821.79 116.00 L 823.55 112.00 L 825.38 108.00 L 827.27 104.00 L 829.23 100.00 L 831.26 96.00 L 833.38 92.00 L 835.58 88.00 L 837.88 84.00 L 840.28 80.00 L 842.78 76.00 L 845.40 72.00 L 848.16 68.00 L 851.07 64.00 L 854.16 60.00 L 857.43 56.00 L 860.91 52.00 L 864.62 48.00 L 868.57 44.00 L 872.77 40.00 L 877.26 36.00 L 882.04 32.00 L 887.13 28.00 L 892.56 24.00 L 893.96 23.00" pathLength="1000"
            stroke="#006BFF" stroke-width="30" opacity="0"
            stroke-dasharray="205 795" stroke-dashoffset="0"
            filter="url(#wideGlow)">
        <animate attributeName="stroke-dashoffset" values="0;-1000" dur="2.85s" begin="-1.55s" repeatCount="indefinite"/>
        <animate attributeName="opacity" values="0;0.24;0.24;0" keyTimes="0;0.08;0.88;1" dur="2.85s" begin="-1.55s" repeatCount="indefinite"/>
      </path>
      <path d="M 782.52 244.00 L 783.30 240.00 L 784.11 236.00 L 784.95 232.00 L 785.81 228.00 L 786.71 224.00 L 787.63 220.00 L 788.59 216.00 L 789.57 212.00 L 790.58 208.00 L 791.62 204.00 L 792.69 200.00 L 793.78 196.00 L 794.91 192.00 L 796.06 188.00 L 797.25 184.00 L 798.46 180.00 L 799.70 176.00 L 800.97 172.00 L 802.26 168.00 L 803.59 164.00 L 804.94 160.00 L 806.32 156.00 L 807.74 152.00 L 809.17 148.00 L 810.64 144.00 L 812.14 140.00 L 813.66 136.00 L 815.21 132.00 L 816.80 128.00 L 818.42 124.00 L 820.08 120.00 L 821.79 116.00 L 823.55 112.00 L 825.38 108.00 L 827.27 104.00 L 829.23 100.00 L 831.26 96.00 L 833.38 92.00 L 835.58 88.00 L 837.88 84.00 L 840.28 80.00 L 842.78 76.00 L 845.40 72.00 L 848.16 68.00 L 851.07 64.00 L 854.16 60.00 L 857.43 56.00 L 860.91 52.00 L 864.62 48.00 L 868.57 44.00 L 872.77 40.00 L 877.26 36.00 L 882.04 32.00 L 887.13 28.00 L 892.56 24.00 L 893.96 23.00" pathLength="1000"
            stroke="#1684FF" stroke-width="9" opacity="0"
            stroke-dasharray="155 845" stroke-dashoffset="0"
            filter="url(#tightGlow)">
        <animate attributeName="stroke-dashoffset" values="0;-1000" dur="2.85s" begin="-1.55s" repeatCount="indefinite"/>
        <animate attributeName="opacity" values="0;0.98;0.98;0" keyTimes="0;0.09;0.86;1" dur="2.85s" begin="-1.55s" repeatCount="indefinite"/>
      </path>
      <path d="M 782.52 244.00 L 783.30 240.00 L 784.11 236.00 L 784.95 232.00 L 785.81 228.00 L 786.71 224.00 L 787.63 220.00 L 788.59 216.00 L 789.57 212.00 L 790.58 208.00 L 791.62 204.00 L 792.69 200.00 L 793.78 196.00 L 794.91 192.00 L 796.06 188.00 L 797.25 184.00 L 798.46 180.00 L 799.70 176.00 L 800.97 172.00 L 802.26 168.00 L 803.59 164.00 L 804.94 160.00 L 806.32 156.00 L 807.74 152.00 L 809.17 148.00 L 810.64 144.00 L 812.14 140.00 L 813.66 136.00 L 815.21 132.00 L 816.80 128.00 L 818.42 124.00 L 820.08 120.00 L 821.79 116.00 L 823.55 112.00 L 825.38 108.00 L 827.27 104.00 L 829.23 100.00 L 831.26 96.00 L 833.38 92.00 L 835.58 88.00 L 837.88 84.00 L 840.28 80.00 L 842.78 76.00 L 845.40 72.00 L 848.16 68.00 L 851.07 64.00 L 854.16 60.00 L 857.43 56.00 L 860.91 52.00 L 864.62 48.00 L 868.57 44.00 L 872.77 40.00 L 877.26 36.00 L 882.04 32.00 L 887.13 28.00 L 892.56 24.00 L 893.96 23.00" pathLength="1000"
            stroke="#C8EEFF" stroke-width="2.4" opacity="0"
            stroke-dasharray="70 930" stroke-dashoffset="0">
        <animate attributeName="stroke-dashoffset" values="0;-1000" dur="2.85s" begin="-1.55s" repeatCount="indefinite"/>
        <animate attributeName="opacity" values="0;1;1;0" keyTimes="0;0.11;0.82;1" dur="2.85s" begin="-1.55s" repeatCount="indefinite"/>
      </path>
    </g>
    <g id="centerL-sweep" class="sweep-route">
      <path d="M 1021.00 519.00 L 1021.00 278.00 L 1335.00 278.00" pathLength="1000"
            stroke="#006BFF" stroke-width="30" opacity="0"
            stroke-dasharray="205 795" stroke-dashoffset="0"
            filter="url(#wideGlow)">
        <animate attributeName="stroke-dashoffset" values="0;-1000" dur="2.70s" begin="-0.80s" repeatCount="indefinite"/>
        <animate attributeName="opacity" values="0;0.24;0.24;0" keyTimes="0;0.08;0.88;1" dur="2.70s" begin="-0.80s" repeatCount="indefinite"/>
      </path>
      <path d="M 1021.00 519.00 L 1021.00 278.00 L 1335.00 278.00" pathLength="1000"
            stroke="#1684FF" stroke-width="9" opacity="0"
            stroke-dasharray="155 845" stroke-dashoffset="0"
            filter="url(#tightGlow)">
        <animate attributeName="stroke-dashoffset" values="0;-1000" dur="2.70s" begin="-0.80s" repeatCount="indefinite"/>
        <animate attributeName="opacity" values="0;0.98;0.98;0" keyTimes="0;0.09;0.86;1" dur="2.70s" begin="-0.80s" repeatCount="indefinite"/>
      </path>
      <path d="M 1021.00 519.00 L 1021.00 278.00 L 1335.00 278.00" pathLength="1000"
            stroke="#C8EEFF" stroke-width="2.4" opacity="0"
            stroke-dasharray="70 930" stroke-dashoffset="0">
        <animate attributeName="stroke-dashoffset" values="0;-1000" dur="2.70s" begin="-0.80s" repeatCount="indefinite"/>
        <animate attributeName="opacity" values="0;1;1;0" keyTimes="0;0.11;0.82;1" dur="2.70s" begin="-0.80s" repeatCount="indefinite"/>
      </path>
    </g>
    <g id="right-sweep" class="sweep-route">
      <path d="M 1463.79 239.00 L 1466.04 243.00 L 1468.25 247.00 L 1470.44 251.00 L 1472.60 255.00 L 1474.73 259.00 L 1476.83 263.00 L 1478.90 267.00 L 1480.94 271.00 L 1482.96 275.00 L 1484.95 279.00 L 1486.91 283.00 L 1488.84 287.00 L 1490.75 291.00 L 1492.64 295.00 L 1494.50 299.00 L 1496.33 303.00 L 1498.14 307.00 L 1499.92 311.00 L 1501.68 315.00 L 1503.42 319.00 L 1505.13 323.00 L 1506.82 327.00 L 1508.48 331.00 L 1510.13 335.00 L 1511.75 339.00 L 1513.35 343.00 L 1514.92 347.00 L 1516.48 351.00 L 1518.01 355.00 L 1519.53 359.00 L 1521.02 363.00 L 1522.49 367.00 L 1523.95 371.00 L 1525.38 375.00 L 1526.80 379.00 L 1528.19 383.00 L 1529.57 387.00 L 1530.93 391.00 L 1532.27 395.00 L 1533.60 399.00 L 1534.90 403.00 L 1536.19 407.00 L 1537.47 411.00 L 1538.72 415.00 L 1539.97 419.00 L 1541.19 423.00 L 1542.40 427.00 L 1543.60 431.00 L 1544.78 435.00 L 1545.95 439.00 L 1547.10 443.00 L 1548.24 447.00 L 1549.37 451.00 L 1550.48 455.00 L 1552.00 460.00 L 1729.00 460.00" pathLength="1000"
            stroke="#006BFF" stroke-width="30" opacity="0"
            stroke-dasharray="205 795" stroke-dashoffset="0"
            filter="url(#wideGlow)">
        <animate attributeName="stroke-dashoffset" values="0;-1000" dur="3.05s" begin="-1.90s" repeatCount="indefinite"/>
        <animate attributeName="opacity" values="0;0.24;0.24;0" keyTimes="0;0.08;0.88;1" dur="3.05s" begin="-1.90s" repeatCount="indefinite"/>
      </path>
      <path d="M 1463.79 239.00 L 1466.04 243.00 L 1468.25 247.00 L 1470.44 251.00 L 1472.60 255.00 L 1474.73 259.00 L 1476.83 263.00 L 1478.90 267.00 L 1480.94 271.00 L 1482.96 275.00 L 1484.95 279.00 L 1486.91 283.00 L 1488.84 287.00 L 1490.75 291.00 L 1492.64 295.00 L 1494.50 299.00 L 1496.33 303.00 L 1498.14 307.00 L 1499.92 311.00 L 1501.68 315.00 L 1503.42 319.00 L 1505.13 323.00 L 1506.82 327.00 L 1508.48 331.00 L 1510.13 335.00 L 1511.75 339.00 L 1513.35 343.00 L 1514.92 347.00 L 1516.48 351.00 L 1518.01 355.00 L 1519.53 359.00 L 1521.02 363.00 L 1522.49 367.00 L 1523.95 371.00 L 1525.38 375.00 L 1526.80 379.00 L 1528.19 383.00 L 1529.57 387.00 L 1530.93 391.00 L 1532.27 395.00 L 1533.60 399.00 L 1534.90 403.00 L 1536.19 407.00 L 1537.47 411.00 L 1538.72 415.00 L 1539.97 419.00 L 1541.19 423.00 L 1542.40 427.00 L 1543.60 431.00 L 1544.78 435.00 L 1545.95 439.00 L 1547.10 443.00 L 1548.24 447.00 L 1549.37 451.00 L 1550.48 455.00 L 1552.00 460.00 L 1729.00 460.00" pathLength="1000"
            stroke="#1684FF" stroke-width="9" opacity="0"
            stroke-dasharray="155 845" stroke-dashoffset="0"
            filter="url(#tightGlow)">
        <animate attributeName="stroke-dashoffset" values="0;-1000" dur="3.05s" begin="-1.90s" repeatCount="indefinite"/>
        <animate attributeName="opacity" values="0;0.98;0.98;0" keyTimes="0;0.09;0.86;1" dur="3.05s" begin="-1.90s" repeatCount="indefinite"/>
      </path>
      <path d="M 1463.79 239.00 L 1466.04 243.00 L 1468.25 247.00 L 1470.44 251.00 L 1472.60 255.00 L 1474.73 259.00 L 1476.83 263.00 L 1478.90 267.00 L 1480.94 271.00 L 1482.96 275.00 L 1484.95 279.00 L 1486.91 283.00 L 1488.84 287.00 L 1490.75 291.00 L 1492.64 295.00 L 1494.50 299.00 L 1496.33 303.00 L 1498.14 307.00 L 1499.92 311.00 L 1501.68 315.00 L 1503.42 319.00 L 1505.13 323.00 L 1506.82 327.00 L 1508.48 331.00 L 1510.13 335.00 L 1511.75 339.00 L 1513.35 343.00 L 1514.92 347.00 L 1516.48 351.00 L 1518.01 355.00 L 1519.53 359.00 L 1521.02 363.00 L 1522.49 367.00 L 1523.95 371.00 L 1525.38 375.00 L 1526.80 379.00 L 1528.19 383.00 L 1529.57 387.00 L 1530.93 391.00 L 1532.27 395.00 L 1533.60 399.00 L 1534.90 403.00 L 1536.19 407.00 L 1537.47 411.00 L 1538.72 415.00 L 1539.97 419.00 L 1541.19 423.00 L 1542.40 427.00 L 1543.60 431.00 L 1544.78 435.00 L 1545.95 439.00 L 1547.10 443.00 L 1548.24 447.00 L 1549.37 451.00 L 1550.48 455.00 L 1552.00 460.00 L 1729.00 460.00" pathLength="1000"
            stroke="#C8EEFF" stroke-width="2.4" opacity="0"
            stroke-dasharray="70 930" stroke-dashoffset="0">
        <animate attributeName="stroke-dashoffset" values="0;-1000" dur="3.05s" begin="-1.90s" repeatCount="indefinite"/>
        <animate attributeName="opacity" values="0;1;1;0" keyTimes="0;0.11;0.82;1" dur="3.05s" begin="-1.90s" repeatCount="indefinite"/>
      </path>
    </g>
  </g>
</svg>
`;
