1. Outside text inserted into the page as HTML (innerHTML, dangerouslySetInnerHTML, v-html) without sanitising: alert.
2. The text is escaped by the framework or sanitised: dismiss.
3. A postMessage handler that trusts any origin: alert.
4. Not shown where the text comes from: investigate.
