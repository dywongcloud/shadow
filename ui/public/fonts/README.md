# ABC Diatype

The platform typeface is ABC Diatype (https://abcdinamo.com/typefaces/diatype).
It is a licensed font and is not bundled. To enable it:

1. Drop these woff2 files here:
   - ABCDiatype-Regular.woff2   (400)
   - ABCDiatype-Medium.woff2    (500)
   - ABCDiatype-Bold.woff2      (700)
   - ABCDiatypeMono-Regular.woff2 (mono 400)
2. Paste this block back into `app/globals.css` (right under the Tailwind
   directives). It was removed while the files are absent because declaring
   the faces without them logged four 404s in every visitor's console:

```css
@font-face {
  font-family: "ABC Diatype";
  src: url("/fonts/ABCDiatype-Regular.woff2") format("woff2");
  font-weight: 400; font-style: normal; font-display: swap;
}
@font-face {
  font-family: "ABC Diatype";
  src: url("/fonts/ABCDiatype-Medium.woff2") format("woff2");
  font-weight: 500; font-style: normal; font-display: swap;
}
@font-face {
  font-family: "ABC Diatype";
  src: url("/fonts/ABCDiatype-Bold.woff2") format("woff2");
  font-weight: 700; font-style: normal; font-display: swap;
}
@font-face {
  font-family: "ABC Diatype Mono";
  src: url("/fonts/ABCDiatypeMono-Regular.woff2") format("woff2");
  font-weight: 400; font-style: normal; font-display: swap;
}
```

The Tailwind font stack already prefers "ABC Diatype" (falling back to Geist
until the files + declarations exist).
