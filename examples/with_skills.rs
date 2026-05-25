//! Skills system: discover, inspect, and inject skills.
//! Creates a temporary skill directory at runtime — no setup required.

use recursive::skills::{discover_skills, skill_index, skills_for_injection, Skill};

fn create_demo_skill(dir: &std::path::Path, name: &str, description: &str, body: &str) {
    let skill_dir = dir.join(name);
    std::fs::create_dir_all(&skill_dir).expect("create skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\nmode: manual\n---\n\n{body}"),
    )
    .expect("write SKILL.md");
}

#[tokio::main]
async fn main() {
    // Create a temporary directory with demo skills.
    let tmp = tempfile::tempdir().expect("create temp dir");

    create_demo_skill(
        tmp.path(),
        "greeter",
        "A skill that knows how to greet people",
        "# Greeter Skill\n\nThis skill provides greeting capabilities.\n\n## Usage\n\nCall `load_skill` with name `greeter`.\n",
    );

    create_demo_skill(
        tmp.path(),
        "math-helper",
        "Helpful math formulas and tips",
        "# Math Helper\n\n## Formulas\n\n- Area of a circle: πr²\n- Pythagorean theorem: a² + b² = c²\n",
    );

    // Discover skills from the temp directory.
    let skills: Vec<Skill> = discover_skills(&[tmp.path().to_path_buf()]);

    println!("Discovered {} skill(s):", skills.len());
    for skill in &skills {
        println!("  - {}: {}", skill.name, skill.description);
    }

    // Generate a skill index (markdown summary).
    let index = skill_index(&skills);
    println!("\nSkill index:\n{index}");

    // Check which skills match a goal (for trigger-mode injection).
    let goal = "Help me with math";
    let matches = skills_for_injection(&skills, goal);
    println!("\nSkills matching goal '{goal}':");
    for (name, hint) in &matches {
        println!("  - {name}: {hint}");
    }
}
