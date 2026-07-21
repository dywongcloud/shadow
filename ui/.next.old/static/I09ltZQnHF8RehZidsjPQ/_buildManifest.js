self.__BUILD_MANIFEST = {
  "__rewrites": {
    "afterFiles": [
      {
        "source": "/cloud/v1/zkauth/:path*",
        "destination": "/api/blocked"
      },
      {
        "source": "/cloud/:path*"
      },
      {
        "source": "/ops/:path*"
      }
    ],
    "beforeFiles": [],
    "fallback": []
  },
  "sortedPages": [
    "/_app",
    "/_error"
  ]
};self.__BUILD_MANIFEST_CB && self.__BUILD_MANIFEST_CB()