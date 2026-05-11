declare module "input" {
  interface InputModule {
    text(message: string): Promise<string>;
    confirm(message: string): Promise<boolean>;
    select<T extends string>(message: string, choices: T[]): Promise<T>;
  }
  const input: InputModule;
  export default input;
}
