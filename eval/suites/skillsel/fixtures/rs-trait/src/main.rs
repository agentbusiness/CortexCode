mod report;

fn main() {
    println!("{}", report::describe(42));
    println!("{}", report::describe("disk"));
    println!("{}", report::summary(&[1.5, 2.25]));
}
