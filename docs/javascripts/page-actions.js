(() => {
  const copyText = async (text) => {
    if (navigator.clipboard && window.isSecureContext) {
      await navigator.clipboard.writeText(text);
      return;
    }

    const textarea = document.createElement("textarea");
    textarea.value = text;
    textarea.setAttribute("readonly", "");
    textarea.style.position = "fixed";
    textarea.style.opacity = "0";
    document.body.appendChild(textarea);
    textarea.select();
    const copied = document.execCommand("copy");
    textarea.remove();
    if (!copied) throw new Error("Clipboard fallback was rejected");
  };

  document.addEventListener("click", async (event) => {
    const button = event.target.closest("[data-ib-copy-page]");
    if (!button) return;

    const idleLabel = "Copy page as Markdown";
    button.disabled = true;

    try {
      const response = await fetch(button.dataset.ibCopyPage, {
        headers: { Accept: "text/markdown, text/plain;q=0.9" },
      });
      if (!response.ok) throw new Error(`Markdown request failed: ${response.status}`);

      await copyText(await response.text());
      button.dataset.mdState = "done";
      button.title = "Markdown copied";
      button.setAttribute("aria-label", "Markdown copied");
    } catch (error) {
      console.error(error);
      button.dataset.mdState = "error";
      button.title = "Could not copy Markdown";
      button.setAttribute("aria-label", "Could not copy Markdown");
    } finally {
      window.setTimeout(() => {
        button.disabled = false;
        delete button.dataset.mdState;
        button.title = idleLabel;
        button.setAttribute("aria-label", idleLabel);
      }, 2000);
    }
  });
})();
