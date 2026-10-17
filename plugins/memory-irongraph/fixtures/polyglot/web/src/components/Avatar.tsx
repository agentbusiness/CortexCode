interface AvatarProps {
  url: string;
  size?: number;
}

/** A round picture of a person. */
export function Avatar({ url, size = 32 }: AvatarProps) {
  return <img src={url} width={size} height={size} />;
}
