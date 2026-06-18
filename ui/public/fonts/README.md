# ABC Diatype

The platform typeface is ABC Diatype (https://abcdinamo.com/typefaces/diatype).
It is a licensed font and is not bundled. To enable it, drop these woff2 files here:

- ABCDiatype-Regular.woff2   (400)
- ABCDiatype-Medium.woff2    (500)
- ABCDiatype-Bold.woff2      (700)
- ABCDiatypeMono-Regular.woff2 (mono 400)

@font-face declarations live in app/globals.css and the Tailwind font stack
already prefers "ABC Diatype" (falling back to Geist until the files exist).
