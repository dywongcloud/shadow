import { redirect } from "next/navigation";

// The Projects list is now the main "Projects" tab (the overview at "/").
export default function ProjectsRedirect() {
  redirect("/");
}
