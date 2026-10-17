for (const question of document.querySelectorAll(".faq .question")) {
  question.addEventListener("click", () => {
    question.nextElementSibling.classList.toggle("open");
  });
}
