import { Request, Response } from "express";
import { OrderService } from "../services/order.service";
import { OrderRepository } from "../repositories/order.repository";
import { OrderStatus } from "../domain/order";

export class OrderController {
  constructor(
    private readonly service: OrderService,
    private readonly orders: OrderRepository,
  ) {}

  async create(req: Request, res: Response): Promise<void> {
    const order = await this.service.createOrder(req.body.customerId, req.body.items);
    res.status(201).json(order);
  }

  async cancel(req: Request, res: Response): Promise<void> {
    await this.service.cancelOrder(req.user, req.params.id);
    res.status(204).end();
  }

  async managerCancel(req: Request, res: Response): Promise<void> {
    const order = await this.orders.findById(req.params.id);
    order.status = OrderStatus.CANCELLED;
    await this.orders.update(order);
    res.status(204).end();
  }
}
