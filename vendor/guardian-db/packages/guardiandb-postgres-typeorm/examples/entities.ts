// TypeORM entities for the package examples.
//
// Ordinary TypeORM entities — there is nothing GuardianDB-specific here, which
// is the point: `GuardianDataSource` is a standard `postgres` DataSource that
// happens to manage its own embedded gateway.

import {
  Entity,
  PrimaryGeneratedColumn,
  Column,
  ManyToOne,
  OneToMany,
  Index,
  CreateDateColumn,
} from "typeorm";

@Entity({ name: "app_user" })
export class User {
  @PrimaryGeneratedColumn()
  id!: number;

  @Index({ unique: true })
  @Column({ type: "varchar", length: 160 })
  email!: string;

  @Column({ type: "text" })
  name!: string;

  // A JSONB column round-trips structured data.
  @Column({ type: "jsonb", default: {} })
  settings!: Record<string, unknown>;

  @OneToMany(() => Post, (post) => post.author)
  posts!: Post[];

  @CreateDateColumn({ type: "timestamptz" })
  createdAt!: Date;
}

@Entity({ name: "post" })
export class Post {
  @PrimaryGeneratedColumn("uuid")
  id!: string;

  @Index()
  @Column({ type: "text" })
  title!: string;

  @Column({ type: "text", nullable: true })
  body!: string | null;

  @Column({ type: "boolean", default: false })
  published!: boolean;

  @ManyToOne(() => User, (user) => user.posts, { onDelete: "CASCADE" })
  author!: User;

  @CreateDateColumn({ type: "timestamptz" })
  createdAt!: Date;
}
